pub mod config;
pub mod error;
pub mod manifest;
pub mod repo;
pub mod selector;
pub mod session;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use config::{AppConfig, DiskConfig, ObjectStoreConfig};
pub use error::{GitCacheError, Result};
pub use manifest::{CommitManifest, GenerationId, GenerationManifest, RefManifest};
pub use repo::{CommitSha, RepoKey, ShortCommitSha};
pub use selector::{BranchName, Selector};
pub use session::{SessionId, SessionManifest};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestMode {
    Strict,
    Cached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializeSource {
    GithubVerified,
    CacheVerified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeRequest {
    pub repo: RepoKey,
    pub selector: Selector,
    #[serde(default = "default_request_mode")]
    pub mode: RequestMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeResponse {
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub source: MaterializeSource,
    pub verified_at: DateTime<Utc>,
    pub git_url: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub expires_at: DateTime<Utc>,
}

fn default_request_mode() -> RequestMode {
    RequestMode::Strict
}

impl Default for RequestMode {
    fn default() -> Self {
        default_request_mode()
    }
}
