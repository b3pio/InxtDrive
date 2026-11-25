//! Public share links, compatible with the Internxt web app.
//!
//! A link carries a short random code. The account's bucket key is stored on
//! the server encrypted with that code, so whoever has the link can decrypt
//! the shared item in the browser; the code itself is stored encrypted with
//! the bucket key so the owner can rebuild the link later.

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::AesGcm;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use reqwest::Method;
use serde::Deserialize;
use serde_json::json;

use crate::client::{Client, Retry};
use crate::drive::ItemKind;
use crate::error::{ApiError, Result};

/// Sharing format the web app currently writes.
pub const SHARING_VERSION: &str = "inxt-v3";
const KDF_ROUNDS: u32 = 2145;
const CODE_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

type Aes256Gcm16 = AesGcm<Aes256, U16>;

fn derive(password: &str, salt: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha2::Sha512>(password.as_bytes(), salt, KDF_ROUNDS, &mut key);
    key
}

/// `@internxt/lib` `aes.encrypt`: PBKDF2-SHA512 key, AES-256-GCM with a
/// 16-byte IV, output `base64(salt[64] || iv[16] || tag[16] || ciphertext)`.
pub fn lib_encrypt(text: &str, password: &str) -> String {
    let mut salt = [0u8; 64];
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut iv);
    lib_encrypt_with(text, password, &salt, &iv)
}

pub fn lib_encrypt_with(text: &str, password: &str, salt: &[u8; 64], iv: &[u8; 16]) -> String {
    let cipher = Aes256Gcm16::new(&derive(password, salt).into());
    let sealed = cipher.encrypt(iv.into(), text.as_bytes()).expect("in-memory AES-GCM cannot fail");
    let (ct, tag) = sealed.split_at(sealed.len() - 16);
    let mut out = Vec::with_capacity(96 + ct.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(iv);
    out.extend_from_slice(tag);
    out.extend_from_slice(ct);
    STANDARD.encode(out)
}

pub fn lib_decrypt(data: &str, password: &str) -> Result<String> {
    let raw = STANDARD.decode(data.trim()).map_err(|e| ApiError::Crypto(format!("base64: {e}")))?;
    if raw.len() <= 96 {
        return Err(ApiError::Crypto("ciphertext too short".into()));
    }
    let (salt, rest) = raw.split_at(64);
    let (iv, rest) = rest.split_at(16);
    let (tag, ct) = rest.split_at(16);
    let mut sealed = ct.to_vec();
    sealed.extend_from_slice(tag);
    let cipher = Aes256Gcm16::new(&derive(password, salt).into());
    let plain = cipher
        .decrypt(iv.into(), sealed.as_slice())
        .map_err(|_| ApiError::Crypto("could not decrypt share data".into()))?;
    String::from_utf8(plain).map_err(|_| ApiError::Crypto("share data is not text".into()))
}

pub fn random_code(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| CODE_CHARS[(*b as usize) % CODE_CHARS.len()] as char).collect()
}

/// UUID → URL-safe base64 of its 16 bytes, as used in share URLs.
pub fn encode_uuid(uuid: &str) -> Result<String> {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    let bytes = hex::decode(&hex).map_err(|_| ApiError::Other("invalid sharing id".into()))?;
    if bytes.len() != 16 {
        return Err(ApiError::Other("invalid sharing id".into()));
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharingMeta {
    id: String,
    #[serde(default)]
    encrypted_code: String,
    #[serde(default)]
    encryption_algorithm: String,
    #[serde(default)]
    encrypted_password: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Domains {
    #[serde(default)]
    list: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ShareLink {
    pub url: String,
    pub sharing_id: String,
    pub code: String,
    pub has_password: bool,
}

/// An item the user shares or that is shared with them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SharedItem {
    pub uuid: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub shared_at: Option<String>,
    pub shared_with_me: bool,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawShared {
    uuid: String,
    #[serde(default)]
    plain_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default, deserialize_with = "crate::types::de_u64_lenient")]
    size: u64,
    #[serde(default)]
    date_shared: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    shared_with_me: bool,
    #[serde(default)]
    user: Option<RawUser>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawUser {
    #[serde(default)]
    email: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SharedPage {
    #[serde(default)]
    files: Vec<RawShared>,
    #[serde(default)]
    folders: Vec<RawShared>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SharingInfo {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    public_sharing: Option<PublicSharing>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PublicSharing {
    id: String,
    #[serde(default)]
    is_password_protected: bool,
    #[serde(default)]
    encrypted_code: String,
}

impl RawShared {
    fn into_item(self, is_dir: bool) -> SharedItem {
        let base = self.plain_name.filter(|s| !s.is_empty()).or(self.name).unwrap_or_else(|| self.uuid.clone());
        let name = match self.file_type.as_deref() {
            Some(t) if !is_dir && !t.is_empty() => format!("{base}.{t}"),
            _ => base,
        };
        SharedItem {
            uuid: self.uuid,
            name,
            is_dir,
            size: self.size,
            shared_at: self.date_shared.or(self.created_at),
            shared_with_me: self.shared_with_me,
            owner: self.user.and_then(|u| u.email),
        }
    }
}

impl Client {
    pub async fn share_domains(&self) -> Vec<String> {
        let r: Result<Domains> =
            self.drive_call("share domains", Method::GET, "/storage/share/domains", &[], None, Retry::Idempotent).await;
        r.map(|d| d.list).unwrap_or_default()
    }

    /// Creates (or returns the existing) public link for an item.
    pub async fn create_public_link(&self, item_uuid: &str, kind: ItemKind) -> Result<ShareLink> {
        let creds = self.credentials()?;
        let bucket_key_hex = self.bucket_key()?.share_key_hex();
        let code = random_code(8);
        let body = json!({
            "itemId": item_uuid,
            "itemType": kind.as_str(),
            "encryptionKey": lib_encrypt(&bucket_key_hex, &code),
            "encryptionAlgorithm": SHARING_VERSION,
            "encryptedCode": lib_encrypt(&code, &bucket_key_hex),
            "persistPreviousSharing": true,
        });
        let sent_code = body["encryptedCode"].as_str().unwrap_or_default().to_string();
        let meta: SharingMeta =
            self.drive_call("create share link", Method::POST, "/sharings", &[], Some(body), Retry::Once).await?;
        let code = if meta.encrypted_code.is_empty() || meta.encrypted_code == sent_code {
            code
        } else if meta.encryption_algorithm == SHARING_VERSION {
            lib_decrypt(&meta.encrypted_code, &bucket_key_hex)?
        } else {
            lib_decrypt(&meta.encrypted_code, &creds.mnemonic)?
        };
        let domains = self.share_domains().await;
        let domain = if domains.is_empty() {
            self.cfg.web_url.clone()
        } else {
            domains[(rand::random::<u32>() as usize) % domains.len()].clone()
        };
        let url = format!(
            "{}/sh/{}/{}/{}",
            domain.trim_end_matches('/'),
            kind.as_str(),
            encode_uuid(&meta.id)?,
            code
        );
        Ok(ShareLink { url, sharing_id: meta.id, code, has_password: meta.encrypted_password.is_some() })
    }

    pub async fn set_share_password(&self, link: &ShareLink, password: Option<&str>) -> Result<()> {
        let path = format!("/sharings/{}/password", link.sharing_id);
        match password.filter(|p| !p.is_empty()) {
            Some(p) => {
                let body = json!({ "encryptedPassword": lib_encrypt(p, &link.code) });
                self.drive_call_empty("set share password", Method::PATCH, &path, Some(body), Retry::Idempotent).await
            }
            None => self.drive_call_empty("remove share password", Method::DELETE, &path, None, Retry::Idempotent).await,
        }
    }

    /// Everything shared by or with this account, folders first.
    pub async fn list_shared(&self) -> Result<Vec<SharedItem>> {
        const PER_PAGE: usize = 50;
        let mut out = Vec::new();
        for (path, is_dir) in [("/sharings/folders", true), ("/sharings/files", false)] {
            for page in 0..40 {
                let q = [("page", page.to_string()), ("perPage", PER_PAGE.to_string())];
                let p: SharedPage = self.drive_call("list shared", Method::GET, path, &q, None, Retry::Idempotent).await?;
                let items = if is_dir { p.folders } else { p.files };
                let n = items.len();
                out.extend(items.into_iter().map(|r| r.into_item(is_dir)));
                if n < PER_PAGE {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Rebuilds the public link of an item that already has one.
    pub async fn existing_public_link(&self, item_uuid: &str, kind: ItemKind) -> Result<Option<ShareLink>> {
        let path = format!("/sharings/{}/{}/info", kind.as_str(), item_uuid);
        let info: SharingInfo = match self.drive_call("sharing info", Method::GET, &path, &[], None, Retry::Idempotent).await {
            Ok(i) => i,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        let Some(public) = info.public_sharing.filter(|_| info.kind != "private") else { return Ok(None) };
        let key = self.bucket_key()?.share_key_hex();
        let code = match lib_decrypt(&public.encrypted_code, &key) {
            Ok(c) => c,
            Err(_) => lib_decrypt(&public.encrypted_code, &self.credentials()?.mnemonic)?,
        };
        let domains = self.share_domains().await;
        let domain = domains.first().cloned().unwrap_or_else(|| self.cfg.web_url.clone());
        let url = format!("{}/sh/{}/{}/{}", domain.trim_end_matches('/'), kind.as_str(), encode_uuid(&public.id)?, code);
        Ok(Some(ShareLink { url, sharing_id: public.id, code, has_password: public.is_password_protected }))
    }

    /// Removes every share of an item (public link and invitations).
    pub async fn stop_sharing(&self, item_uuid: &str, kind: ItemKind) -> Result<()> {
        let path = format!("/sharings/{}/{}", kind.as_str(), item_uuid);
        self.drive_call_empty("stop sharing", Method::DELETE, &path, None, Retry::Idempotent).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lib_aes_roundtrip() {
        let c = lib_encrypt("bucket-key-hex", "Ab3_-x9Q");
        assert_eq!(lib_decrypt(&c, "Ab3_-x9Q").unwrap(), "bucket-key-hex");
        assert!(lib_decrypt(&c, "wrong").is_err());
    }

    #[test]
    fn codes_and_ids() {
        let c = random_code(8);
        assert_eq!(c.len(), 8);
        assert!(c.bytes().all(|b| CODE_CHARS.contains(&b)));
        assert_eq!(encode_uuid("f32a91da-c799-4e13-aa17-8c4d9e0323c9").unwrap(), "8yqR2seZThOqF4xNngMjyQ");
    }
}
