pub mod auth;
pub mod config;
pub mod error;
pub mod manifest;
pub mod repo;
pub mod selector;
pub mod update;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use auth::{SecretString, UpstreamAuth, UpstreamAuthorizationMode};
pub use config::{
    default_max_concurrent_git_processes, AppConfig, BranchRefCheck, CompactionConfig, DiskConfig,
    GitRemoteConfig, LeaseConfig, ObjectStoreConfig,
};
pub use error::{GitCacheError, Result};
pub use manifest::{
    CommitManifest, GenerationId, GenerationManifest, RefManifest, RepoGenerationHead,
    VerifiedGenerationManifest,
};
pub use repo::{CommitSha, RepoKey, ShortCommitSha};
pub use selector::{BranchName, ReachabilitySelector, Selector};
pub use update::{
    validate_event_ref, UpdateDisposition, UpdateExecutor, UpdateKey, UpdateOutcome, UpdateRequest,
    UpdateResult, UpdateSource, UpdateTarget,
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
    #[serde(alias = "github_verified")]
    UpstreamVerified,
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

impl MaterializeRequest {
    pub fn requires_upstream_auth(&self) -> bool {
        self.upstream_authorization.is_required()
    }

    pub fn uses_upstream_auth(&self, auth: &UpstreamAuth) -> bool {
        auth.is_authenticated() || self.requires_upstream_auth()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeResponse {
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub source: MaterializeSource,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveResponse {
    pub repo: RepoKey,
    pub selector: Selector,
    pub commit: CommitSha,
    pub source: MaterializeSource,
    pub cache_available: bool,
    pub authorized_at: DateTime<Utc>,
}

fn default_request_mode() -> RequestMode {
    RequestMode::Strict
}

impl Default for RequestMode {
    fn default() -> Self {
        default_request_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialize_source_uses_provider_neutral_public_label() {
        let serialized = serde_json::to_string(&MaterializeSource::UpstreamVerified).unwrap();
        assert_eq!(serialized, "\"upstream_verified\"");

        let parsed: MaterializeSource = serde_json::from_str("\"github_verified\"").unwrap();
        assert_eq!(parsed, MaterializeSource::UpstreamVerified);
    }
}
