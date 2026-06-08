use crate::state::AppState;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
#[cfg(test)]
use git_cache_core::RequestMode;
use git_cache_core::{
    BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest, GitCacheError,
    MaterializeRequest, MaterializeResponse, MaterializeSource, RefManifest, RepoGenerationHead,
    RepoKey, ResolveResponse, Result as CoreResult, Selector, ShortCommitSha, UpstreamAuth,
    VerifiedGenerationManifest,
};
use git_cache_core::{UpdateExecutor, UpdateRequest, UpdateResult, UpdateTarget};
use git_cache_disk::RepoLock;
pub use git_cache_git::UploadPackProcess;
use git_cache_objectstore::{
    generation_manifest_key, pending_generation_publish_key, read_commit_manifest,
    read_generation_manifest, read_json, read_lease_with_version, read_pending_generation_publish,
    read_repo_generation_head, read_verified_generation_manifest, verified_generation_manifest_key,
    write_commit_manifest, write_json, write_ref_manifest, write_repo_generation_head,
    write_verified_generation_manifest_if_absent_or_matches, GenerationPublish,
    PendingGenerationPublish, PublishManifests,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};
use tokio::fs;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

mod access;
mod direct_git;
mod executor;
mod generations;
mod manifests;
mod planning;
mod repo;
mod util;

pub use direct_git::{
    frame_ref_advertisement, synthesize_ref_advertisement, upload_pack_has_wants,
    UpstreamRefComparison,
};
pub use executor::MaterializerExecutor;
pub use generations::{bundle_key, default_manifest_key, CompactionReport};
pub use repo::repo_from_git_path;

#[derive(Clone)]
pub struct Materializer {
    state: Arc<AppState>,
    upstream_auth: UpstreamAuth,
    lease_token: Option<String>,
}

impl Materializer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            upstream_auth: UpstreamAuth::Anonymous,
            lease_token: None,
        }
    }

    pub fn with_lease_token(state: Arc<AppState>, token: String) -> Self {
        Self {
            state,
            upstream_auth: UpstreamAuth::Anonymous,
            lease_token: Some(token),
        }
    }

    pub fn using_upstream_auth(&self, auth: &UpstreamAuth) -> Self {
        Self {
            state: Arc::clone(&self.state),
            upstream_auth: auth.clone(),
            lease_token: self.lease_token.clone(),
        }
    }

    /// Verify that the repo-write lease is still held by this worker before
    /// mutating shared object-store state. This is a no-op for materializers
    /// created without a worker lease token.
    async fn verify_lease_held(&self, repo: &RepoKey) -> CoreResult<()> {
        let Some(expected) = &self.lease_token else {
            return Ok(());
        };
        let lease_name = "repo-write";
        match read_lease_with_version(&*self.state.store, repo, lease_name).await? {
            Some((manifest, version)) => {
                if manifest.token != *expected {
                    return Err(GitCacheError::LeaseLost(format!(
                        "repo-write lease for {repo} was taken over by another worker"
                    )));
                }
                if manifest.released_at.is_some() {
                    return Err(GitCacheError::LeaseLost(format!(
                        "repo-write lease for {repo} was released"
                    )));
                }
                let now = Utc::now();
                let renewed_at_by_holder = manifest.renewed_at.unwrap_or(manifest.acquired_at);
                let ttl_at_write = manifest.expires_at - renewed_at_by_holder;
                let expired = if ttl_at_write <= chrono::Duration::zero() {
                    true
                } else if let Some(obj_updated) = version.updated_at {
                    now - obj_updated >= ttl_at_write
                } else {
                    now >= manifest.expires_at
                };
                if expired {
                    return Err(GitCacheError::LeaseLost(format!(
                        "repo-write lease for {repo} expired before shared-state write"
                    )));
                }
                Ok(())
            }
            None => Err(GitCacheError::LeaseLost(format!(
                "repo-write lease for {repo} was released"
            ))),
        }
    }
}

pub(super) fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests;
