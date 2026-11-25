use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crypto::BucketKey;
use crate::error::{ApiError, Result};
use crate::types::RefreshResponse;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// API gateway hosting `/drive` and `/network`.
    pub gateway_url: String,
    /// Web app used for browser sign-in.
    pub web_url: String,
    /// Sent as `internxt-client`; identifies this application to the API.
    pub client_name: String,
    pub client_version: String,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            gateway_url: "https://gateway.internxt.com".into(),
            web_url: "https://drive.internxt.com".into(),
            client_name: "inxtdrive".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// Everything needed to act on behalf of a signed-in user. Holds secrets:
/// never log it and only persist it encrypted.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub token: String,
    pub mnemonic: String,
    pub email: String,
    pub user_uuid: String,
    pub user_id: String,
    pub bridge_user: String,
    pub bucket: String,
    pub root_folder_uuid: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("user_uuid", &self.user_uuid).finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Safe to repeat: retried on throttling, server errors and network failures.
    Idempotent,
    /// Only retried when the request provably never reached the server.
    Once,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Auth {
    Bearer,
    Bridge,
    None,
}

type CredsListener = Box<dyn Fn(&Credentials) + Send + Sync>;

pub struct Client {
    pub(crate) http: reqwest::Client,
    pub(crate) cfg: ClientConfig,
    creds: RwLock<Option<Arc<Credentials>>>,
    bucket_key: RwLock<Option<BucketKey>>,
    refresh_gate: tokio::sync::Mutex<()>,
    on_creds_changed: RwLock<Option<CredsListener>>,
}

const MAX_ATTEMPTS: u32 = 6;

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "internxt-client",
            HeaderValue::from_str(&cfg.client_name).map_err(|e| ApiError::Other(e.to_string()))?,
        );
        headers.insert(
            "internxt-version",
            HeaderValue::from_str(&cfg.client_version).map_err(|e| ApiError::Other(e.to_string()))?,
        );
        let http = reqwest::Client::builder()
            .user_agent(format!("{}/{}", cfg.client_name, cfg.client_version))
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| ApiError::Transport { op: "http client", source: e })?;
        Ok(Self {
            http,
            cfg,
            creds: RwLock::new(None),
            bucket_key: RwLock::new(None),
            refresh_gate: tokio::sync::Mutex::new(()),
            on_creds_changed: RwLock::new(None),
        })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    pub fn set_credentials(&self, creds: Option<Credentials>) -> Result<()> {
        let key = match &creds {
            Some(c) => Some(BucketKey::derive(&c.mnemonic, &c.bucket)?),
            None => None,
        };
        *self.bucket_key.write() = key;
        *self.creds.write() = creds.map(Arc::new);
        Ok(())
    }

    /// Called whenever the session token is rotated, so it can be persisted.
    pub fn on_credentials_changed(&self, f: impl Fn(&Credentials) + Send + Sync + 'static) {
        *self.on_creds_changed.write() = Some(Box::new(f));
    }

    pub fn credentials(&self) -> Result<Arc<Credentials>> {
        self.creds.read().clone().ok_or(ApiError::NotSignedIn)
    }

    pub fn bucket_key(&self) -> Result<BucketKey> {
        self.bucket_key.read().clone().ok_or(ApiError::NotSignedIn)
    }

    pub(crate) fn drive_url(&self, path: &str) -> String {
        format!("{}/drive{}", self.cfg.gateway_url.trim_end_matches('/'), path)
    }

    pub(crate) fn network_url(&self, path: &str) -> String {
        format!("{}/network{}", self.cfg.gateway_url.trim_end_matches('/'), path)
    }

    fn bridge_auth(creds: &Credentials) -> String {
        let pass = hex::encode(Sha256::digest(creds.user_id.as_bytes()));
        format!("Basic {}", STANDARD.encode(format!("{}:{}", creds.bridge_user, pass)))
    }

    /// Sends a request built by `build`, retrying per `retry` and refreshing
    /// the session once on a 401.
    pub(crate) async fn execute(
        &self,
        op: &'static str,
        auth: Auth,
        retry: Retry,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<Response> {
        let mut attempt = 0u32;
        let mut refreshed = false;
        loop {
            attempt += 1;
            let mut rb = build();
            match auth {
                Auth::Bearer => rb = rb.bearer_auth(&self.credentials()?.token),
                Auth::Bridge => rb = rb.header("Authorization", Self::bridge_auth(&*self.credentials()?)),
                Auth::None => {}
            }
            match rb.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if status == 401 && auth == Auth::Bearer && !refreshed {
                        refreshed = true;
                        Box::pin(self.refresh_session()).await?;
                        continue;
                    }
                    if status == 401 && auth != Auth::None {
                        return Err(ApiError::Unauthorized { op });
                    }
                    let retry_after = parse_retry_after(resp.headers());
                    let mut body = resp.text().await.unwrap_or_default();
                    body.truncate(600);
                    let err = ApiError::Http { op, status, body, retry_after };
                    let may_retry = match retry {
                        Retry::Idempotent => err.is_transient(),
                        Retry::Once => status == 429,
                    };
                    if may_retry && attempt < MAX_ATTEMPTS {
                        tokio::time::sleep(retry_after.unwrap_or_else(|| backoff(attempt))).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) => {
                    let may_retry = match retry {
                        Retry::Idempotent => true,
                        Retry::Once => e.is_connect(),
                    };
                    if may_retry && attempt < MAX_ATTEMPTS {
                        tracing::debug!(op, attempt, error = %e, "retrying after transport error");
                        tokio::time::sleep(backoff(attempt)).await;
                        continue;
                    }
                    return Err(ApiError::Transport { op, source: e });
                }
            }
        }
    }

    pub(crate) async fn json<T: DeserializeOwned>(&self, op: &'static str, resp: Response) -> Result<T> {
        let bytes = resp.bytes().await.map_err(|e| ApiError::Transport { op, source: e })?;
        serde_json::from_slice(&bytes).map_err(|e| {
            let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned();
            ApiError::Decode(op, format!("{e}; body starts {snippet:?}"))
        })
    }

    pub(crate) async fn drive_call<T: DeserializeOwned>(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<serde_json::Value>,
        retry: Retry,
    ) -> Result<T> {
        let url = self.drive_url(path);
        let resp = self
            .execute(op, Auth::Bearer, retry, || {
                let mut rb = self.http.request(method.clone(), &url).query(query);
                if let Some(b) = &body {
                    rb = rb.json(b);
                }
                rb
            })
            .await?;
        self.json(op, resp).await
    }

    pub(crate) async fn drive_call_empty(
        &self,
        op: &'static str,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        retry: Retry,
    ) -> Result<()> {
        let url = self.drive_url(path);
        self.execute(op, Auth::Bearer, retry, || {
            let mut rb = self.http.request(method.clone(), &url);
            if let Some(b) = &body {
                rb = rb.json(b);
            }
            rb
        })
        .await?;
        Ok(())
    }

    /// Rotates the session token. Concurrent callers share one refresh.
    pub async fn refresh_session(&self) -> Result<()> {
        let before = self.credentials()?.token.clone();
        let _gate = self.refresh_gate.lock().await;
        if self.credentials()?.token != before {
            return Ok(());
        }
        let url = self.drive_url("/users/refresh");
        let resp = self
            .execute("refresh session", Auth::None, Retry::Idempotent, || {
                self.http.get(&url).bearer_auth(&before)
            })
            .await
            .map_err(|e| match e.status() {
                Some(401) | Some(403) => ApiError::Unauthorized { op: "refresh session" },
                _ => e,
            })?;
        let refreshed: RefreshResponse = self.json("refresh session", resp).await?;
        let mut creds = (*self.credentials()?).clone();
        creds.token = refreshed.new_token;
        if !refreshed.user.bucket.is_empty() {
            creds.bucket = refreshed.user.bucket;
        }
        if !refreshed.user.root_folder_id.is_empty() {
            creds.root_folder_uuid = refreshed.user.root_folder_id;
        }
        self.set_credentials(Some(creds.clone()))?;
        if let Some(cb) = self.on_creds_changed.read().as_ref() {
            cb(&creds);
        }
        tracing::info!("session token refreshed");
        Ok(())
    }

    /// Refreshes the token when it expires within `margin`.
    pub async fn ensure_fresh(&self, margin: chrono::Duration) -> Result<()> {
        let token = self.credentials()?.token.clone();
        match crate::jwt::expiry(&token) {
            Some(exp) if exp - chrono::Utc::now() > margin => Ok(()),
            _ => self.refresh_session().await,
        }
    }
}

fn backoff(attempt: u32) -> Duration {
    let ms = 500u64.saturating_mul(1 << attempt.min(6));
    Duration::from_millis(ms.min(20_000) + rand::random::<u64>() % 250)
}

fn parse_retry_after(h: &HeaderMap) -> Option<Duration> {
    let v = h.get(RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(|s| Duration::from_secs(s.min(120)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct Empty {}
