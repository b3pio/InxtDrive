use std::io::SeekFrom;
use std::path::Path;
use std::sync::Arc;

use aes::cipher::StreamCipher;
use bytes::Bytes;
use futures_util::StreamExt;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::client::{Auth, Client, Retry};
use crate::crypto::{self, FileKey};
use crate::error::{ApiError, Result};
use crate::types::{BucketFileInfo, FinishUploadResponse, Mirror, StartUploadResponse};

/// Files at or above this size use S3 multipart upload.
pub const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;
pub const MULTIPART_CHUNK: u64 = 30 * 1024 * 1024;
const MULTIPART_PARALLEL: usize = 3;
const STREAM_CHUNK: usize = 256 * 1024;

/// Receives decrypted bytes at their absolute file offset.
pub type Sink<'a> = dyn FnMut(u64, &[u8]) -> std::io::Result<()> + Send + 'a;

impl Client {
    pub async fn bucket_file_info(&self, file_id: &str) -> Result<BucketFileInfo> {
        let bucket = self.credentials()?.bucket.clone();
        let url = self.network_url(&format!("/buckets/{bucket}/files/{file_id}/info"));
        let resp = self
            .execute("file info", Auth::Bridge, Retry::Idempotent, || {
                self.http.get(&url).header("internxt-version", "1.0")
            })
            .await?;
        self.json("file info", resp).await
    }

    pub fn file_key(&self, info: &BucketFileInfo) -> Result<FileKey> {
        self.bucket_key()?.file_key(&info.index)
    }

    /// Downloads and decrypts `len` bytes starting at `start`.
    pub async fn download_range(&self, info: &BucketFileInfo, start: u64, len: u64, sink: &mut Sink<'_>) -> Result<()> {
        if len == 0 || start >= info.size {
            return Ok(());
        }
        let len = len.min(info.size - start);
        let key = self.file_key(info)?;
        if info.is_legacy() {
            return self.download_legacy(info, &key, start, len, sink).await;
        }
        let shard = info.shards.first().ok_or_else(|| ApiError::Decode("file info", "no shards".into()))?;
        let range = format!("bytes={}-{}", start, start + len - 1);
        let resp = self
            .execute("download", Auth::None, Retry::Idempotent, || {
                self.http.get(&shard.url).header("Range", &range)
            })
            .await?;
        let mut skip = match resp.status() {
            StatusCode::PARTIAL_CONTENT => 0,
            _ => start,
        };
        let mut cipher = key.cipher_at(start);
        let mut pos = start;
        let end = start + len;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ApiError::Transport { op: "download", source: e })?;
            let mut data: &[u8] = &chunk;
            if skip > 0 {
                let n = (skip as usize).min(data.len());
                data = &data[n..];
                skip -= n as u64;
            }
            if data.is_empty() {
                continue;
            }
            let take = ((end - pos) as usize).min(data.len());
            let mut plain = data[..take].to_vec();
            cipher.apply_keystream(&mut plain);
            sink(pos, &plain)?;
            pos += take as u64;
            if pos >= end {
                break;
            }
        }
        if pos < end {
            return Err(ApiError::Integrity(format!("download ended early at {pos} of {end}")));
        }
        Ok(())
    }

    async fn mirrors(&self, file_id: &str) -> Result<Vec<Mirror>> {
        let bucket = self.credentials()?.bucket.clone();
        let mut all = Vec::new();
        let mut skip = 0;
        loop {
            let url = self.network_url(&format!("/buckets/{bucket}/files/{file_id}?limit=3&skip={skip}"));
            let resp = self.execute("mirrors", Auth::Bridge, Retry::Idempotent, || self.http.get(&url)).await?;
            let page: Vec<Mirror> = self.json("mirrors", resp).await?;
            if page.is_empty() {
                break;
            }
            skip += page.len();
            all.extend(page.into_iter().filter(|m| !m.parity));
            if skip > 100_000 {
                break;
            }
        }
        all.sort_by_key(|m| m.index);
        Ok(all)
    }

    /// Pre-2022 files are split across several shards on storage nodes.
    async fn download_legacy(&self, info: &BucketFileInfo, key: &FileKey, start: u64, len: u64, sink: &mut Sink<'_>) -> Result<()> {
        let mirrors = self.mirrors(&info.id).await?;
        let mut shard_start = 0u64;
        let end = start + len;
        for m in mirrors {
            let shard_end = shard_start + m.size;
            if shard_end <= start {
                shard_start = shard_end;
                continue;
            }
            if shard_start >= end {
                break;
            }
            if m.url.is_empty() {
                return Err(ApiError::Other(format!("legacy shard {} has no storage node", m.index)));
            }
            let resp = self.execute("legacy download", Auth::None, Retry::Idempotent, || self.http.get(&m.url)).await?;
            let body = resp.bytes().await.map_err(|e| ApiError::Transport { op: "legacy download", source: e })?;
            let mut cipher = key.cipher_at(shard_start);
            let mut plain = body.to_vec();
            cipher.apply_keystream(&mut plain);
            let from = start.saturating_sub(shard_start) as usize;
            let to = ((end - shard_start) as usize).min(plain.len());
            if from < to {
                sink(shard_start + from as u64, &plain[from..to])?;
            }
            shard_start = shard_end;
        }
        Ok(())
    }

    /// Encrypts and uploads a local plaintext file, returning the network file id.
    pub async fn upload_file(&self, path: &Path, size: u64, progress: Arc<dyn Fn(u64) + Send + Sync>) -> Result<String> {
        if size == 0 {
            return Err(ApiError::Other("empty files have no network content".into()));
        }
        let index = crypto::random_index_hex();
        let key = self.bucket_key()?.file_key(&index)?;
        if size >= MULTIPART_THRESHOLD {
            self.upload_multipart(path, size, &index, &key, progress).await
        } else {
            self.upload_single(path, size, &index, &key, progress).await
        }
    }

    async fn start_upload(&self, size: u64, parts: u64) -> Result<crate::types::UploadSlot> {
        let bucket = self.credentials()?.bucket.clone();
        let url = self.network_url(&format!("/v2/buckets/{bucket}/files/start?multiparts={parts}"));
        let body = json!({ "uploads": [{ "index": 0, "size": size }] });
        let resp = self
            .execute("start upload", Auth::Bridge, Retry::Idempotent, || {
                self.http.post(&url).header("internxt-version", "1.0").json(&body)
            })
            .await?;
        let r: StartUploadResponse = self.json("start upload", resp).await?;
        r.uploads.into_iter().next().ok_or_else(|| ApiError::Decode("start upload", "no upload slot".into()))
    }

    async fn finish_upload(&self, index: &str, shard: serde_json::Value) -> Result<String> {
        let bucket = self.credentials()?.bucket.clone();
        let url = self.network_url(&format!("/v2/buckets/{bucket}/files/finish"));
        let body = json!({ "index": index, "shards": [shard] });
        let resp = self
            .execute("finish upload", Auth::Bridge, Retry::Idempotent, || {
                self.http.post(&url).header("internxt-version", "1.0").json(&body)
            })
            .await?;
        let r: FinishUploadResponse = self.json("finish upload", resp).await?;
        Ok(r.id)
    }

    async fn upload_single(&self, path: &Path, size: u64, index: &str, key: &FileKey, progress: Arc<dyn Fn(u64) + Send + Sync>) -> Result<String> {
        let slot = self.start_upload(size, 1).await?;
        let url = slot
            .urls
            .as_ref()
            .and_then(|u| u.first().cloned())
            .or(slot.url.clone())
            .ok_or_else(|| ApiError::Decode("start upload", "no upload url".into()))?;

        let mut attempt = 0;
        let hash = loop {
            attempt += 1;
            let hasher = Arc::new(Mutex::new(Sha256::new()));
            let body = encrypted_body(path.to_path_buf(), 0, size, key.clone(), hasher.clone(), progress.clone()).await?;
            let res = self
                .http
                .put(&url)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", size)
                .body(body)
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {
                    let h = std::mem::take(&mut *hasher.lock());
                    break crypto::content_hash(h);
                }
                Ok(r) if attempt < 4 && (r.status().is_server_error() || r.status() == 429) => {}
                Ok(r) => {
                    let status = r.status().as_u16();
                    let body = r.text().await.unwrap_or_default();
                    return Err(ApiError::Http { op: "upload", status, body, retry_after: None });
                }
                Err(e) if attempt < 4 => tracing::debug!(error = %e, "upload attempt failed"),
                Err(e) => return Err(ApiError::Transport { op: "upload", source: e }),
            }
            tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
        };
        self.finish_upload(index, json!({ "hash": hash, "uuid": slot.uuid })).await
    }

    async fn upload_multipart(&self, path: &Path, size: u64, index: &str, key: &FileKey, progress: Arc<dyn Fn(u64) + Send + Sync>) -> Result<String> {
        let parts = size.div_ceil(MULTIPART_CHUNK);
        let slot = self.start_upload(size, parts).await?;
        let urls = slot.urls.clone().unwrap_or_default();
        let upload_id = slot.upload_id.clone().ok_or_else(|| ApiError::Decode("start upload", "no multipart id".into()))?;
        if urls.len() as u64 != parts {
            return Err(ApiError::Decode("start upload", format!("expected {parts} part urls, got {}", urls.len())));
        }

        let mut file = tokio::fs::File::open(path).await?;
        let mut overall = Sha256::new();
        let sem = Arc::new(tokio::sync::Semaphore::new(MULTIPART_PARALLEL));
        let mut tasks = tokio::task::JoinSet::new();
        let mut completed = Vec::with_capacity(parts as usize);
        for (i, url) in urls.into_iter().enumerate() {
            let offset = i as u64 * MULTIPART_CHUNK;
            let len = MULTIPART_CHUNK.min(size - offset) as usize;
            let permit = sem.clone().acquire_owned().await.expect("semaphore open");
            let mut buf = vec![0u8; len];
            file.read_exact(&mut buf).await?;
            key.cipher_at(offset).apply_keystream(&mut buf);
            overall.update(&buf);
            let data = Bytes::from(buf);
            let http = self.http.clone();
            let progress = progress.clone();
            tasks.spawn(async move {
                let _permit = permit;
                let etag = put_part(&http, &url, data).await?;
                progress(len as u64);
                Ok::<_, ApiError>((i + 1, etag))
            });
            while let Some(done) = tasks.try_join_next() {
                completed.push(done.map_err(|e| ApiError::Other(e.to_string()))??);
            }
        }
        while let Some(done) = tasks.join_next().await {
            completed.push(done.map_err(|e| ApiError::Other(e.to_string()))??);
        }
        completed.sort_by_key(|(n, _)| *n);
        let parts_json: Vec<_> = completed.iter().map(|(n, e)| json!({ "PartNumber": n, "ETag": e })).collect();
        let hash = crypto::content_hash(overall);
        self.finish_upload(index, json!({ "uuid": slot.uuid, "hash": hash, "UploadId": upload_id, "parts": parts_json }))
            .await
    }
}

async fn put_part(http: &reqwest::Client, url: &str, data: Bytes) -> Result<String> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let res = http
            .put(url)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", data.len())
            .body(data.clone())
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                let etag = r
                    .headers()
                    .get("ETag")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                return Ok(etag);
            }
            Ok(r) if attempt < 5 && (r.status().is_server_error() || r.status() == 429) => {}
            Ok(r) => {
                let status = r.status().as_u16();
                let body = r.text().await.unwrap_or_default();
                return Err(ApiError::Http { op: "upload part", status, body, retry_after: None });
            }
            Err(e) if attempt < 5 => tracing::debug!(error = %e, "part upload attempt failed"),
            Err(e) => return Err(ApiError::Transport { op: "upload part", source: e }),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
    }
}

/// Streams `len` bytes of `path` from `offset`, encrypted, hashing the ciphertext.
async fn encrypted_body(
    path: std::path::PathBuf,
    offset: u64,
    len: u64,
    key: FileKey,
    hasher: Arc<Mutex<Sha256>>,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<reqwest::Body> {
    let mut file = tokio::fs::File::open(&path).await?;
    file.seek(SeekFrom::Start(offset)).await?;
    let cipher = key.cipher_at(offset);
    let stream = futures_util::stream::unfold((file, cipher, len), move |(mut file, mut cipher, remaining)| {
        let hasher = hasher.clone();
        let progress = progress.clone();
        async move {
            if remaining == 0 {
                return None;
            }
            let want = (remaining as usize).min(STREAM_CHUNK);
            let mut buf = vec![0u8; want];
            if let Err(e) = file.read_exact(&mut buf).await {
                return Some((Err(e), (file, cipher, 0)));
            }
            cipher.apply_keystream(&mut buf);
            hasher.lock().update(&buf);
            progress(want as u64);
            Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (file, cipher, remaining - want as u64)))
        }
    });
    Ok(reqwest::Body::wrap_stream(stream))
}
