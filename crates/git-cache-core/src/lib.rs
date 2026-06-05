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
use std::fmt;

pub use auth::{SecretString, UpstreamAuth, UpstreamAuthorizationMode};
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
pub use selector::{BranchName, ReachabilitySelector, Selector};
pub use session::{SessionId, SessionManifest, SessionProtection};
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

impl MaterializeRequest {
    pub fn requires_upstream_auth(&self) -> bool {
        self.upstream_authorization.is_required()
    }

    pub fn uses_upstream_auth(&self, auth: &UpstreamAuth) -> bool {
        auth.is_authenticated() || self.requires_upstream_auth()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializeResponse {
    pub repo: RepoKey,
    pub commit: CommitSha,
    pub source: MaterializeSource,
    pub verified_at: DateTime<Utc>,
    pub git_url: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

impl fmt::Debug for MaterializeResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let session_token = self.session_token.as_ref().map(|_| "<redacted>");
        f.debug_struct("MaterializeResponse")
            .field("repo", &self.repo)
            .field("commit", &self.commit)
            .field("source", &self.source)
            .field("verified_at", &self.verified_at)
            .field("git_url", &self.git_url)
            .field("ref_name", &self.ref_name)
            .field("session_token", &session_token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
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
    fn materialize_response_debug_redacts_session_token() {
        let response = MaterializeResponse {
            repo: RepoKey::parse("github.com/org/repo").unwrap(),
            commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
            source: MaterializeSource::UpstreamAuthorizedFetched,
            verified_at: Utc::now(),
            git_url: "https://cache.example/git/session/session/github.com/org/repo.git".into(),
            ref_name: "refs/heads/main".into(),
            session_token: Some("gcs_secret_session_token".into()),
            expires_at: Utc::now(),
        };

        let debug = format!("{response:?}");

        assert!(
            debug.contains("session_token: Some(\"<redacted>\")"),
            "{debug}"
        );
        assert!(!debug.contains("gcs_secret_session_token"), "{debug}");
    }
}
