use crate::state::AppState;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{
    BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest, GitCacheError,
    MaterializeRequest, MaterializeResponse, MaterializeSource, PackInfo, PackKind, RefManifest,
    RepoGenerationHead, RepoKey, ResolveResponse, Result as CoreResult, Selector, ShortCommitSha,
    UpstreamAuth,
};
use git_cache_core::{UpdateExecutor, UpdateRequest, UpdateTarget};
use git_cache_disk::RepoLock;
pub use git_cache_git::UploadPackProcess;
use git_cache_objectstore::{
    generation_manifest_key, generation_manifest_prefix, pack_key, pack_prefix,
    read_commit_manifest, read_generation_manifest, read_json, read_repo_generation_head,
    read_repo_generation_head_versioned, write_commit_manifest, write_json, write_ref_manifest,
    write_repo_generation_head_if_version_matches, GenerationPublish, ObjectVersion,
    PublishManifests,
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
mod proxy_tee;
mod repo;

pub use direct_git::{
    frame_ref_advertisement, synthesize_ref_advertisement, UpstreamRefComparison,
};
pub use executor::MaterializerExecutor;
pub use generations::{default_manifest_key, CompactionReport, GenerationSweepReport};
pub use proxy_tee::{plan_upload_pack_tee, PackDemux, UploadPackTeePlan};
pub use repo::repo_from_git_path;

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

pub(super) fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

#[cfg(test)]
mod tests;
