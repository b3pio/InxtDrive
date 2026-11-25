use std::time::Duration;

pub type Result<T> = std::result::Result<T, ApiError>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{op}: HTTP {status}: {body}")]
    Http {
        op: &'static str,
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    #[error("{op}: session expired or revoked")]
    Unauthorized { op: &'static str },
    #[error("{op}: transport error: {source}")]
    Transport {
        op: &'static str,
        #[source]
        source: reqwest::Error,
    },
    #[error("{0}: unexpected response: {1}")]
    Decode(&'static str, String),
    #[error("crypto: {0}")]
    Crypto(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not signed in")]
    NotSignedIn,
    #[error("{0}")]
    Other(String),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Http { status, .. } => Some(*status),
            ApiError::Unauthorized { .. } => Some(401),
            _ => None,
        }
    }

    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    pub fn is_conflict(&self) -> bool {
        self.status() == Some(409)
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, ApiError::Unauthorized { .. } | ApiError::NotSignedIn)
    }

    /// Errors worth retrying later without any change on our side.
    pub fn is_transient(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => *status == 408 || *status == 429 || *status >= 500,
            ApiError::Transport { .. } => true,
            ApiError::Io(_) => true,
            _ => false,
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            ApiError::Http { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    pub fn body_contains(&self, needle: &str) -> bool {
        match self {
            ApiError::Http { body, .. } => body.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()),
            _ => false,
        }
    }
}
