use thiserror::Error;

pub type Result<T> = std::result::Result<T, GitCacheError>;

#[derive(Debug, Error)]
pub enum GitCacheError {
    #[error("invalid input: {0}")]
    Validation(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("upstream unavailable: {0}")]
    UpstreamUnavailable(String),
    #[error("insufficient local disk: {0}")]
    DiskFull(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("operation timed out: {0}")]
    Timeout(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("not implemented yet: {0}")]
    NotImplemented(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
