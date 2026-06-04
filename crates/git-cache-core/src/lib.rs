pub mod auth;
pub mod config;
pub mod error;
pub mod manifest;
pub mod repo;
pub mod selector;
pub mod session;
pub mod update;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use auth::{
    ReachabilitySelector, SessionProtection, UpstreamAuth, UpstreamAuthorizationMode,
};
pub use config::{
    default_max_concurrent_git_processes, AppConfig, BranchRefCheck, CompactionConfig, DiskConfig,
    GitRemoteConfig, ObjectStoreConfig,
};
pub use error::{GitCacheError, Result};
pub use manifest::{
    CommitManifest, GenerationId, GenerationManifest, RefManifest, RepoGenerationHead,
    VerifiedGenerationManifest,
};
pub use repo::{CommitSha, RepoKey, ShortCommitSha};
pub use selector::{BranchName, Selector};
pub use session::{SessionId, SessionManifest};
pub use update::{
    validate_event_ref, UpdateDisposition, UpdateExecutor, UpdateKey, UpdateOutcome, UpdateRequest,
    UpdateSource, UpdateTarget,
};

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
    UpstreamAuthorizedCacheHit,
    UpstreamAuthorizedFetched,
    PublicCacheHit,
    PublicFetched,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeRequest {
    pub repo: RepoKey,
    pub selector: Selector,
    #[serde(default = "default_request_mode")]
    pub mode: RequestMode,
    #[serde(default)]
    pub upstream_authorization: UpstreamAuthorizationMode,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

fn default_request_mode() -> RequestMode {
    RequestMode::Strict
}

impl Default for RequestMode {
    fn default() -> Self {
        default_request_mode()
    }
}
