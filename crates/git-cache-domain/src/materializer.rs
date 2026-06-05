use crate::state::AppState;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use git_cache_core::{
    BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest, GitCacheError,
    MaterializeRequest, MaterializeResponse, MaterializeSource, ReachabilitySelector, RefManifest,
    RepoGenerationHead, RepoKey, RequestMode, ResolveResponse, Result as CoreResult, Selector,
    SessionId, SessionManifest, SessionProtection, ShortCommitSha, UpstreamAuth,
    VerifiedGenerationManifest,
};
use git_cache_core::{UpdateExecutor, UpdateRequest, UpdateTarget};
use git_cache_disk::RepoLock;
pub use git_cache_git::UploadPackProcess;
use git_cache_objectstore::{
    generation_manifest_key, pending_generation_publish_key, read_commit_manifest,
    read_generation_manifest, read_json, read_pending_generation_publish,
    read_repo_generation_head, read_session_manifest, read_verified_generation_manifest,
    verified_generation_manifest_key, write_commit_manifest, write_json, write_ref_manifest,
    write_repo_generation_head, write_session_manifest,
    write_verified_generation_manifest_if_absent_or_matches, GenerationPublish, PublishManifests,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration as StdDuration;
use tokio::fs;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};
use uuid::Uuid;

mod direct_git;
mod executor;
mod generations;
mod manifests;
mod planning;
mod repo;
mod sessions;
mod util;

pub use direct_git::{
    advertise_refs, frame_ref_advertisement, parse_want_lines,
    synthesize_protected_ref_advertisement, synthesize_ref_advertisement, upload_pack,
    UpstreamRefComparison,
};
pub use executor::MaterializerExecutor;
pub use generations::{bundle_key, default_manifest_key, CompactionReport};
pub use repo::repo_from_git_path;
pub use sessions::SessionCleanupReport;

#[derive(Clone)]
pub struct Materializer {
    state: Arc<AppState>,
    upstream_auth: UpstreamAuth,
}

impl Materializer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            upstream_auth: UpstreamAuth::Anonymous,
        }
    }

    pub fn using_upstream_auth(&self, auth: &UpstreamAuth) -> Self {
        Self {
            state: Arc::clone(&self.state),
            upstream_auth: auth.clone(),
        }
    }
}

#[cfg(test)]
mod tests;
