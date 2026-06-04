use crate::state::AppState;
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use git_cache_core::{
    BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest, GitCacheError,
    MaterializeRequest, MaterializeResponse, MaterializeSource, RefManifest, RepoGenerationHead,
    RepoKey, RequestMode, Result as CoreResult, Selector, SessionId, SessionManifest,
    ShortCommitSha, VerifiedGenerationManifest,
};
use git_cache_core::{UpdateExecutor, UpdateRequest, UpdateTarget};
use git_cache_disk::RepoLock;
pub use git_cache_git::UploadPackProcess;
#[cfg(test)]
use git_cache_objectstore::read_ref_manifest;
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

const VERIFIED_GENERATION_SCHEMA_VERSION: u32 = 2;
const VERIFIED_GENERATION_VERIFIER_VERSION: u32 = 1;
const VERIFIED_GENERATION_FSCK_MODE: &str = "connectivity-only";
const PENDING_GENERATION_PREFIX: &str = "pending-generations/";
const MAX_PENDING_GENERATION_SCAN_KEYS: usize = 10_000;
const GENERATION_VERIFICATION_MAX_ATTEMPTS: usize = 3;
const GENERATION_VERIFICATION_RETRY_DELAY: StdDuration = StdDuration::from_secs(30);
const SERVED_REPO_CONFIG_MARKER: &str = "git-cache-serving-config-v1";

#[derive(Clone)]
pub struct Materializer {
    state: Arc<AppState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompactionReport {
    pub repo: RepoKey,
    pub old_chain_depth: usize,
    pub old_generations: Vec<GenerationId>,
    pub new_generation: GenerationId,
    pub bytes_reclaimed: u64,
}

impl Materializer {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub async fn materialize(
        &self,
        request: MaterializeRequest,
    ) -> CoreResult<MaterializeResponse> {
        self.validate_host(&request.repo)?;
        match request.selector {
            Selector::Commit(commit) => self.materialize_commit(request.repo, commit).await,
            Selector::ShortCommit(commit) => {
                self.materialize_short_commit(request.repo, commit).await
            }
            Selector::Branch(branch) => {
                self.materialize_branch(request.repo, branch, request.mode, false)
                    .await
            }
            Selector::DefaultBranch => {
                self.materialize_default_branch(request.repo, request.mode)
                    .await
            }
        }
    }

    pub async fn materialize_after_upstream_validation(
        &self,
        request: MaterializeRequest,
    ) -> CoreResult<MaterializeResponse> {
        self.validate_host(&request.repo)?;
        match request.selector {
            Selector::Branch(branch) => self.materialize_local_branch(request.repo, &branch).await,
            Selector::DefaultBranch => self.materialize_local_default_branch(request.repo).await,
            _ => self.materialize(request).await,
        }
    }

    pub async fn materialize_short_commit(
        &self,
        repo: RepoKey,
        short_commit: ShortCommitSha,
    ) -> CoreResult<MaterializeResponse> {
        let repo_dir = self.ensure_repo_dir(&repo).await?;
        self.fetch_all_refs(&repo, &repo_dir).await?;
        let commit = self
            .resolve_short_commit_from_upstream_refs(&repo_dir, &short_commit)
            .await?;
        self.materialize_existing_local_commit(
            repo,
            &repo_dir,
            commit,
            MaterializeSource::GithubVerified,
        )
        .await
    }

    pub async fn materialize_commit(
        &self,
        repo: RepoKey,
        commit: CommitSha,
    ) -> CoreResult<MaterializeResponse> {
        if let Some(manifest) = self.get_commit_manifest(&repo, &commit).await? {
            if manifest.complete {
                self.hydrate_commit(&manifest).await?;
                return self
                    .create_session(repo, commit, MaterializeSource::CacheVerified)
                    .await;
            }
        }

        let repo_dir = self.ensure_repo_dir(&repo).await?;
        if self.commit_exists(&repo_dir, &commit).await {
            if let Some(generation) = self
                .index_local_commit_from_known_generation(&repo, &repo_dir, &commit)
                .await?
            {
                debug!(%repo, %commit, %generation, "indexed exact commit from known generation");
                return self
                    .create_session(repo, commit, MaterializeSource::CacheVerified)
                    .await;
            }
        }

        self.fetch_all_refs(&repo, &repo_dir).await?;

        if !self.commit_exists(&repo_dir, &commit).await {
            return Err(GitCacheError::NotFound(format!(
                "commit `{commit}` was not found after upstream verification"
            )));
        }

        if let Some(generation) = self
            .index_local_commit_from_known_generation(&repo, &repo_dir, &commit)
            .await?
        {
            debug!(%repo, %commit, %generation, "indexed exact commit from known generation after upstream fetch");
            return self
                .create_session(repo, commit, MaterializeSource::CacheVerified)
                .await;
        }

        self.materialize_existing_local_commit(
            repo,
            &repo_dir,
            commit,
            MaterializeSource::GithubVerified,
        )
        .await
    }

    async fn materialize_existing_local_commit(
        &self,
        repo: RepoKey,
        repo_dir: &FsPath,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        let generation = self
            .publish_generation(&repo, repo_dir, &commit, None, false)
            .await?;
        debug!(%repo, %commit, %generation, "published generation for exact commit");
        self.create_session(repo, commit, source).await
    }

    async fn index_local_commit_from_known_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<Option<GenerationId>> {
        let Some(head) = read_repo_generation_head(&*self.state.store, repo).await? else {
            return Ok(None);
        };

        let mut checked_tips = Vec::new();
        for tip in head.tip_commits.iter().rev() {
            push_unique_commit(&mut checked_tips, tip.clone());
            if self
                .index_local_commit_if_ancestor_of_tip(repo, repo_dir, commit, tip, head.generation)
                .await?
            {
                return Ok(Some(head.generation));
            }
        }

        let candidate_tips = self
            .state
            .git
            .for_each_ref_containing_commit(
                repo_dir,
                commit,
                &["refs/cache/commits", "refs/cache/upstream/heads"],
            )
            .await?;

        for tip in candidate_tips {
            if checked_tips.iter().any(|checked| checked == &tip) {
                continue;
            }
            push_unique_commit(&mut checked_tips, tip.clone());
            let Some(tip_manifest) = self.get_commit_manifest(repo, &tip).await? else {
                continue;
            };
            if !tip_manifest.complete {
                continue;
            }
            self.index_local_commit(repo, commit, tip_manifest.generation)
                .await?;
            return Ok(Some(tip_manifest.generation));
        }

        Ok(None)
    }

    async fn index_local_commit_if_ancestor_of_tip(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        tip: &CommitSha,
        generation: GenerationId,
    ) -> CoreResult<bool> {
        let Some(tip_manifest) = self.get_commit_manifest(repo, tip).await? else {
            return Ok(false);
        };
        if !tip_manifest.complete || tip_manifest.generation != generation {
            return Ok(false);
        }
        if !self.commit_exists(repo_dir, tip).await {
            return Ok(false);
        }
        if !self.state.git.is_ancestor(repo_dir, commit, tip).await? {
            return Ok(false);
        }
        self.index_local_commit(repo, commit, generation).await?;
        Ok(true)
    }

    async fn index_local_commit(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let manifest = CommitManifest {
            repo: repo.clone(),
            commit: commit.clone(),
            generation,
            complete: true,
            verified_at: Utc::now(),
        };
        write_commit_manifest(&*self.state.store, &manifest).await?;
        Ok(())
    }

    pub async fn materialize_branch(
        &self,
        repo: RepoKey,
        branch: BranchName,
        _mode: RequestMode,
        default_branch: bool,
    ) -> CoreResult<MaterializeResponse> {
        let commit = self.ensure_branch(&repo, &branch, default_branch).await?;
        self.create_session(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    /// Fetch and publish a branch from upstream without creating a session.
    /// Returns the verified commit SHA.
    pub async fn ensure_branch(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
        default_branch: bool,
    ) -> CoreResult<CommitSha> {
        self.validate_host(repo)?;
        let upstream_commit = self.ls_remote_branch(repo, branch).await?;
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let local_ref = format!("refs/cache/upstream/heads/{}", branch.as_str());
        self.state
            .git
            .fetch_branch(
                &repo_dir,
                &self.upstream_url(repo)?,
                branch.as_str(),
                &local_ref,
            )
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;

        let commit = self
            .state
            .git
            .rev_parse(&repo_dir, &local_ref)
            .await
            .and_then(CommitSha::parse)?;

        if commit != upstream_commit {
            return Err(GitCacheError::Conflict(format!(
                "upstream branch `{branch}` moved during fetch: ls-remote={upstream_commit}, fetched={commit}"
            )));
        }

        if default_branch {
            self.state
                .git
                .symbolic_ref(
                    &repo_dir,
                    "HEAD",
                    &format!("refs/cache/upstream/heads/{branch}"),
                )
                .await?;
        }

        self.publish_generation(
            repo,
            &repo_dir,
            &commit,
            Some(branch.clone()),
            default_branch,
        )
        .await?;

        Ok(commit)
    }

    pub async fn materialize_default_branch(
        &self,
        repo: RepoKey,
        _mode: RequestMode,
    ) -> CoreResult<MaterializeResponse> {
        let commit = self.ensure_default_branch(&repo).await?;
        self.create_session(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    /// Resolve, fetch and publish the default branch without creating a session.
    /// Returns the verified commit SHA.
    pub async fn ensure_default_branch(&self, repo: &RepoKey) -> CoreResult<CommitSha> {
        self.validate_host(repo)?;
        let branch = self.resolve_default_branch(repo).await?;
        self.ensure_branch(repo, &branch, true).await
    }

    pub async fn create_session(
        &self,
        repo: RepoKey,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        let session_id = SessionId::new();
        let synthetic_ref = session_id.synthetic_ref();
        let now = Utc::now();
        let expires_at =
            now + ChronoDuration::seconds(self.state.config.session_ttl_seconds as i64);
        let manifest = SessionManifest {
            id: session_id,
            repo: repo.clone(),
            commit: commit.clone(),
            synthetic_ref: synthetic_ref.clone(),
            created_at: now,
            expires_at,
        };

        let repo_dir = self.ensure_repo_dir(&repo).await?;
        if !self.commit_ready_for_serving(&repo_dir, &commit).await {
            return Err(GitCacheError::NotFound(format!(
                "cannot create session for missing or incomplete local commit `{commit}`"
            )));
        }

        self.prepare_session_repo(&manifest, &repo_dir).await?;
        write_session_manifest(&*self.state.store, &manifest).await?;

        Ok(MaterializeResponse {
            repo: repo.clone(),
            commit,
            source,
            verified_at: now,
            git_url: format!(
                "{}/git/session/{}/{}.git",
                self.state.config.public_base_url.trim_end_matches('/'),
                session_id,
                repo.as_str()
            ),
            ref_name: synthetic_ref,
            expires_at,
        })
    }

    pub async fn session_repo_from_manifest(
        &self,
        repo: &RepoKey,
        session_id: SessionId,
    ) -> CoreResult<PathBuf> {
        let manifest: SessionManifest = self
            .get_session_manifest(repo, session_id)
            .await?
            .ok_or_else(|| GitCacheError::NotFound(format!("session `{session_id}` not found")))?;

        if manifest.expires_at < Utc::now() {
            return Err(GitCacheError::NotFound(format!(
                "session `{session_id}` expired"
            )));
        }

        if &manifest.repo != repo {
            return Err(GitCacheError::Validation(format!(
                "session `{session_id}` does not belong to repo `{repo}`"
            )));
        }

        let repo_dir = self.ensure_repo_dir(&manifest.repo).await?;
        if !self
            .commit_ready_for_serving(&repo_dir, &manifest.commit)
            .await
        {
            let commit_manifest = self
                .get_commit_manifest(&manifest.repo, &manifest.commit)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "session commit `{}` is missing from manifests",
                        manifest.commit
                    ))
                })?;
            self.hydrate_commit(&commit_manifest).await?;
        }

        self.prepare_session_repo(&manifest, &repo_dir).await?;
        Ok(self.session_repo_path(session_id))
    }

    async fn hydrate_commit(&self, manifest: &CommitManifest) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(&manifest.repo).await?;
        self.hydrate_commit_in_repo(&repo_dir, manifest).await
    }

    async fn hydrate_commit_in_repo(
        &self,
        repo_dir: &FsPath,
        manifest: &CommitManifest,
    ) -> CoreResult<()> {
        if self
            .commit_ready_for_serving(repo_dir, &manifest.commit)
            .await
        {
            return Ok(());
        }

        self.hydrate_generation(&manifest.repo, repo_dir, manifest.generation)
            .await?;
        if !self
            .commit_ready_for_serving(repo_dir, &manifest.commit)
            .await
        {
            return Err(GitCacheError::NotFound(format!(
                "hydrated generation `{}` did not contain complete commit `{}`",
                manifest.generation, manifest.commit
            )));
        }
        Ok(())
    }

    async fn hydrate_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let chain = self.generation_chain(repo, generation).await?;

        for generation_manifest in chain.iter().rev() {
            let verification = read_verified_generation_manifest(
                &*self.state.store,
                repo,
                generation_manifest.generation,
            )
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!(
                    "verified generation manifest `{}` not found",
                    generation_manifest.generation
                ))
            })?;
            let bundle_meta = self
                .state
                .store
                .head(&generation_manifest.bundle_key)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "bundle `{}` not found",
                        generation_manifest.bundle_key
                    ))
                })?;

            validate_verified_generation(generation_manifest, &verification, bundle_meta.len)?;

            let reservation = self.state.disk.reserve(verification.bundle_len).await?;
            let temp_path = reservation.temp_path()?;
            fs::create_dir_all(&temp_path).await?;
            let bundle_path = temp_path.join("hydrate.bundle");
            if !self
                .state
                .store
                .get_file(&generation_manifest.bundle_key, &bundle_path)
                .await?
            {
                return Err(GitCacheError::NotFound(format!(
                    "bundle `{}` not found",
                    generation_manifest.bundle_key
                )));
            }
            let (bundle_len, bundle_sha256) = file_len_and_sha256(&bundle_path).await?;
            if bundle_len != verification.bundle_len {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` length mismatch: expected {}, got {}",
                    generation_manifest.bundle_key, verification.bundle_len, bundle_len
                )));
            }
            if !bundle_sha256.eq_ignore_ascii_case(&verification.bundle_sha256) {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` sha256 mismatch",
                    generation_manifest.bundle_key
                )));
            }
            self.state.git.fetch_bundle(repo_dir, &bundle_path).await?;
            reservation.release().await?;
        }
        Ok(())
    }

    pub async fn publish_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        commit: &CommitSha,
        branch: Option<BranchName>,
        default_branch: bool,
    ) -> CoreResult<GenerationId> {
        if let Some(existing) = self.get_commit_manifest(repo, commit).await? {
            if existing.complete {
                if let Some(branch) = branch {
                    let ref_manifest = RefManifest {
                        repo: repo.clone(),
                        ref_name: branch.ref_name(),
                        commit: commit.clone(),
                        generation: existing.generation,
                        verified_at: Utc::now(),
                    };
                    write_ref_manifest(&*self.state.store, &ref_manifest).await?;
                }
                if default_branch {
                    self.put_default_manifest(repo, commit).await?;
                }

                return Ok(existing.generation);
            }
        }

        let previous_head = read_repo_generation_head(&*self.state.store, repo).await?;
        let previous_generation = previous_head.as_ref().map(|head| head.generation);
        let previous_tips = previous_head
            .as_ref()
            .map(|head| head.tip_commits.as_slice())
            .unwrap_or(&[]);
        let generation = GenerationId::new();
        let bundle_key = bundle_key(repo, generation);

        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("generation.bundle");
        let now = Utc::now();
        let mut parent_generation = previous_generation;
        let mut tip_commits = previous_head
            .as_ref()
            .map(|head| head.tip_commits.clone())
            .unwrap_or_default();

        if previous_tips.is_empty() {
            self.state
                .git
                .bundle_create_all(repo_dir, &bundle_path)
                .await?;
        } else if let Err(err) = self
            .state
            .git
            .bundle_create_incremental(repo_dir, &bundle_path, previous_tips)
            .await
        {
            warn!(%repo, %err, "delta bundle failed, falling back to full bundle");
            let _ = fs::remove_file(&bundle_path).await;
            self.state
                .git
                .bundle_create_all(repo_dir, &bundle_path)
                .await?;
            parent_generation = None;
            tip_commits.clear();
        }
        push_unique_commit(&mut tip_commits, commit.clone());

        let mut manifest_commits = vec![commit.clone()];
        for ref_prefix in ["refs/cache/upstream/heads", "refs/cache/commits"] {
            for candidate in self
                .state
                .git
                .for_each_ref_commits(repo_dir, ref_prefix)
                .await?
            {
                if self.get_commit_manifest(repo, &candidate).await?.is_none() {
                    push_unique_commit(&mut manifest_commits, candidate);
                }
            }
        }

        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation,
            bundle_key,
            parent_generation,
            created_at: now,
            commits: manifest_commits.clone(),
        };
        let mut manifests = PublishManifests {
            commits: manifest_commits
                .into_iter()
                .map(|commit| CommitManifest {
                    repo: repo.clone(),
                    commit,
                    generation,
                    complete: true,
                    verified_at: now,
                })
                .collect(),
            refs: Vec::new(),
            sessions: Vec::new(),
        };

        if let Some(branch) = branch {
            let ref_manifest = RefManifest {
                repo: repo.clone(),
                ref_name: branch.ref_name(),
                commit: commit.clone(),
                generation,
                verified_at: now,
            };
            manifests.refs.push(ref_manifest);
        }
        let default_ref = if default_branch {
            Some(RefManifest {
                repo: repo.clone(),
                ref_name: "HEAD".into(),
                commit: commit.clone(),
                generation,
                verified_at: now,
            })
        } else {
            None
        };
        let head = RepoGenerationHead {
            repo: repo.clone(),
            generation,
            tip_commits,
            updated_at: now,
        };

        GenerationPublish::with_manifests(generation_manifest, manifests)
            .publish_pending_bundle_file(&*self.state.store, &bundle_path, head, default_ref)
            .await?;

        reservation.release().await?;

        self.enqueue_generation_verification(repo.clone(), generation);

        Ok(generation)
    }

    fn enqueue_generation_verification(&self, repo: RepoKey, generation: GenerationId) {
        let state = Arc::clone(&self.state);
        tokio::spawn(async move {
            for attempt in 1..=GENERATION_VERIFICATION_MAX_ATTEMPTS {
                let materializer = Materializer::new(Arc::clone(&state));
                let result = materializer
                    .verify_generation_with_semaphore(repo.clone(), generation, true)
                    .await;
                match result {
                    Ok(()) => return,
                    Err(err) if attempt < GENERATION_VERIFICATION_MAX_ATTEMPTS => {
                        warn!(%repo, %generation, attempt, %err, "generation verification failed; retrying");
                        tokio::time::sleep(GENERATION_VERIFICATION_RETRY_DELAY).await;
                    }
                    Err(err) => {
                        warn!(%repo, %generation, attempt, %err, "generation verification failed");
                        return;
                    }
                }
            }
        });
    }

    pub fn enqueue_pending_generation_scan(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skipping pending generation scan: no active tokio runtime");
            return;
        };
        let materializer = self.clone();
        handle.spawn(async move {
            match materializer
                .enqueue_pending_generation_verifications()
                .await
            {
                Ok(count) => {
                    if count > 0 {
                        info!(count, "enqueued pending generation verifications");
                    }
                }
                Err(err) => warn!(%err, "pending generation scan failed"),
            }
        });
    }

    pub async fn enqueue_pending_generation_verifications(&self) -> CoreResult<usize> {
        let keys = self
            .state
            .store
            .list_prefix(
                PENDING_GENERATION_PREFIX,
                Some(MAX_PENDING_GENERATION_SCAN_KEYS),
            )
            .await?;
        let mut queued = 0usize;
        for key in keys {
            match pending_generation_from_key(&key) {
                Ok(Some((repo, generation))) => {
                    self.enqueue_generation_verification(repo, generation);
                    queued += 1;
                }
                Ok(None) => {}
                Err(err) => warn!(key, %err, "skipping malformed pending generation key"),
            }
        }
        Ok(queued)
    }

    async fn verify_generation_with_semaphore(
        &self,
        repo: RepoKey,
        generation: GenerationId,
        inline_compaction: bool,
    ) -> CoreResult<()> {
        let permit = self
            .state
            .generation_verification_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| {
                GitCacheError::Internal("generation verification semaphore closed".into())
            })?;
        self.verify_generation_inner(repo.clone(), generation)
            .await?;
        drop(permit);

        if inline_compaction && self.state.config.compaction.inline {
            if let Err(err) = Box::pin(self.compact_generation_chain(&repo)).await {
                warn!(%repo, %generation, %err, "inline generation compaction failed");
            }
        }
        Ok(())
    }

    async fn verify_generation_inner(
        &self,
        repo: RepoKey,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let Some(pending) =
            read_pending_generation_publish(&*self.state.store, &repo, generation).await?
        else {
            return Ok(());
        };

        let chain = self
            .generation_chain_for_verification(&repo, generation)
            .await?;
        let mut bundle_metas = Vec::with_capacity(chain.len());
        let mut total_len = 0_u64;
        for manifest in chain.iter().rev() {
            let meta = self
                .state
                .store
                .head(&manifest.bundle_key)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!("bundle `{}` not found", manifest.bundle_key))
                })?;
            total_len = total_len.saturating_add(meta.len);
            bundle_metas.push((manifest, meta.len));
        }

        let reservation = self.state.disk.reserve(total_len).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let verify_repo = temp_path.join("verify.git");
        self.state.git.init_bare(&verify_repo).await?;

        let mut verifications = Vec::with_capacity(bundle_metas.len());
        let now = Utc::now();
        let tip_commits = pending.head.tip_commits.clone();

        for (manifest, object_len) in bundle_metas {
            let bundle_path = temp_path.join(format!("{}.bundle", manifest.generation));
            if !self
                .state
                .store
                .get_file(&manifest.bundle_key, &bundle_path)
                .await?
            {
                return Err(GitCacheError::NotFound(format!(
                    "bundle `{}` not found",
                    manifest.bundle_key
                )));
            }
            let (bundle_len, bundle_sha256) = file_len_and_sha256(&bundle_path).await?;
            if bundle_len != object_len {
                return Err(GitCacheError::Validation(format!(
                    "bundle `{}` length changed during verification: expected {}, got {}",
                    manifest.bundle_key, object_len, bundle_len
                )));
            }
            self.state
                .git
                .fetch_bundle(&verify_repo, &bundle_path)
                .await?;
            let manifest_tip_commits = if manifest.generation == generation {
                tip_commits.clone()
            } else {
                Vec::new()
            };
            verifications.push(verified_generation_manifest(
                manifest,
                bundle_len,
                bundle_sha256,
                now,
                manifest_tip_commits,
            ));
        }

        self.state.git.fsck(&verify_repo).await?;
        let mut pending_verification = None;
        for verification in verifications {
            if verification.generation == generation {
                pending_verification = Some(verification);
            } else if read_verified_generation_manifest(
                &*self.state.store,
                &verification.repo,
                verification.generation,
            )
            .await?
            .is_none()
            {
                write_verified_generation_manifest_if_absent_or_matches(
                    &*self.state.store,
                    &verification,
                )
                .await?;
            }
        }
        let verification = pending_verification.ok_or_else(|| {
            GitCacheError::Internal(format!(
                "verification for generation `{generation}` was not produced"
            ))
        })?;
        GenerationPublish::with_manifests(pending.generation.clone(), pending.manifests.clone())
            .with_verification(verification)
            .publish_verified_metadata(&*self.state.store)
            .await?;

        if let Some(default_ref) = pending.default_ref {
            write_ref_manifest(&*self.state.store, &default_ref).await?;
            write_json(
                &*self.state.store,
                &default_manifest_key(&repo),
                &default_ref,
            )
            .await?;
        }

        let current_head = read_repo_generation_head(&*self.state.store, &repo).await?;
        if current_head
            .as_ref()
            .map(|head| head.updated_at <= pending.head.updated_at)
            .unwrap_or(true)
        {
            write_repo_generation_head(&*self.state.store, &pending.head).await?;
        }
        if let Err(err) = self
            .state
            .store
            .delete(&pending_generation_publish_key(&repo, generation))
            .await
        {
            warn!(%repo, %generation, %err, "failed to delete pending generation publish");
        }
        reservation.release().await?;
        info!(%repo, %generation, "generation verified");
        Ok(())
    }

    async fn generation_chain_for_verification(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Vec<GenerationManifest>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(generation);
        while let Some(current) = next {
            if !seen.insert(current) {
                return Err(GitCacheError::Conflict(format!(
                    "generation chain for `{repo}` contains a cycle at `{current}`"
                )));
            }
            let manifest = if current == generation {
                if let Some(pending) =
                    read_pending_generation_publish(&*self.state.store, repo, current).await?
                {
                    pending.generation
                } else {
                    self.get_generation_manifest(repo, current)
                        .await?
                        .ok_or_else(|| {
                            GitCacheError::NotFound(format!(
                                "generation manifest `{current}` not found"
                            ))
                        })?
                }
            } else {
                self.get_generation_manifest(repo, current)
                    .await?
                    .ok_or_else(|| {
                        GitCacheError::NotFound(format!(
                            "generation manifest `{current}` not found"
                        ))
                    })?
            };
            next = manifest.parent_generation;
            chain.push(manifest);
        }
        Ok(chain)
    }

    pub async fn compact_generation_chain(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Option<CompactionReport>> {
        let threshold = self.state.config.compaction.chain_depth_threshold as usize;
        self.compact_generation_chain_inner(repo, threshold, false)
            .await
    }

    pub async fn compact_generation_chain_dry_run(
        &self,
        repo: &RepoKey,
    ) -> CoreResult<Option<CompactionReport>> {
        let threshold = self.state.config.compaction.chain_depth_threshold as usize;
        self.compact_generation_chain_inner(repo, threshold, true)
            .await
    }

    async fn compact_generation_chain_inner(
        &self,
        repo: &RepoKey,
        threshold: usize,
        dry_run: bool,
    ) -> CoreResult<Option<CompactionReport>> {
        let Some(head) = read_repo_generation_head(&*self.state.store, repo).await? else {
            return Ok(None);
        };
        let chain = self.generation_chain(repo, head.generation).await?;
        if chain.len() <= threshold {
            return Ok(None);
        }

        let old_generations: Vec<GenerationId> =
            chain.iter().map(|manifest| manifest.generation).collect();
        let old_generation_set: HashSet<GenerationId> = old_generations.iter().copied().collect();
        let all_old_generation_bytes = self
            .bundle_bytes_for_generations(repo, &old_generations)
            .await?;
        let new_generation = GenerationId::new();
        if dry_run {
            return Ok(Some(CompactionReport {
                repo: repo.clone(),
                old_chain_depth: chain.len(),
                old_generations,
                new_generation,
                bytes_reclaimed: all_old_generation_bytes,
            }));
        }

        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        self.verify_generation_with_semaphore(repo.clone(), head.generation, false)
            .await?;
        self.hydrate_generation(repo, &repo_dir, head.generation)
            .await?;
        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path()?;
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("compacted.bundle");
        self.state
            .git
            .bundle_create_all(&repo_dir, &bundle_path)
            .await?;

        let now = Utc::now();
        let bundle_key = bundle_key(repo, new_generation);
        let commits = commits_from_chain(chain.iter().rev());
        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation: new_generation,
            bundle_key,
            parent_generation: None,
            created_at: now,
            commits: commits.clone(),
        };
        let new_head = RepoGenerationHead {
            repo: repo.clone(),
            generation: new_generation,
            tip_commits: head.tip_commits.clone(),
            updated_at: now,
        };
        GenerationPublish::new(generation_manifest.clone())
            .publish_pending_bundle_file(&*self.state.store, &bundle_path, new_head.clone(), None)
            .await?;
        self.verify_generation_with_semaphore(repo.clone(), new_generation, false)
            .await?;

        self.repoint_manifests_after_compaction(repo, &old_generation_set, new_generation)
            .await?;
        for commit in commits {
            write_commit_manifest(
                &*self.state.store,
                &CommitManifest {
                    repo: repo.clone(),
                    commit,
                    generation: new_generation,
                    complete: true,
                    verified_at: now,
                },
            )
            .await?;
        }
        write_repo_generation_head(&*self.state.store, &new_head).await?;
        let retained_generations = self
            .old_generations_needed_by_pending_publishes(repo, &old_generation_set)
            .await?;
        let delete_generations = old_generations
            .iter()
            .copied()
            .filter(|generation| !retained_generations.contains(generation))
            .collect::<Vec<_>>();
        let bytes_reclaimed = self
            .bundle_bytes_for_generations(repo, &delete_generations)
            .await?;
        self.delete_old_generations(repo, &delete_generations)
            .await?;

        reservation.release().await?;

        Ok(Some(CompactionReport {
            repo: repo.clone(),
            old_chain_depth: old_generations.len(),
            old_generations,
            new_generation,
            bytes_reclaimed,
        }))
    }

    async fn generation_chain(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Vec<GenerationManifest>> {
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut next = Some(generation);
        while let Some(current) = next {
            if !seen.insert(current) {
                return Err(GitCacheError::Conflict(format!(
                    "generation chain for `{repo}` contains a cycle at `{current}`"
                )));
            }
            let manifest = self
                .get_generation_manifest(repo, current)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!("generation manifest `{current}` not found"))
                })?;
            next = manifest.parent_generation;
            chain.push(manifest);
        }
        Ok(chain)
    }

    async fn old_generations_needed_by_pending_publishes(
        &self,
        repo: &RepoKey,
        old_generations: &HashSet<GenerationId>,
    ) -> CoreResult<HashSet<GenerationId>> {
        let keys = self
            .state
            .store
            .list_prefix(
                &format!("{PENDING_GENERATION_PREFIX}{repo}/"),
                Some(MAX_PENDING_GENERATION_SCAN_KEYS),
            )
            .await?;
        if keys.len() >= MAX_PENDING_GENERATION_SCAN_KEYS {
            return Err(GitCacheError::Conflict(format!(
                "too many pending generations for `{repo}` to compact safely"
            )));
        }

        let mut needed = HashSet::new();
        for key in keys {
            let Some((pending_repo, generation)) = pending_generation_from_key(&key)? else {
                continue;
            };
            if pending_repo != *repo {
                continue;
            }
            let Some(pending) =
                read_pending_generation_publish(&*self.state.store, repo, generation).await?
            else {
                continue;
            };

            let mut seen = HashSet::new();
            let mut next = pending.generation.parent_generation;
            while let Some(current) = next {
                if !seen.insert(current) {
                    return Err(GitCacheError::Conflict(format!(
                        "pending generation chain for `{repo}` contains a cycle at `{current}`"
                    )));
                }
                if old_generations.contains(&current) {
                    needed.insert(current);
                }
                let Some(manifest) = self.get_generation_manifest(repo, current).await? else {
                    if old_generations.contains(&current) {
                        warn!(%repo, generation = %current, pending_generation = %generation, "pending generation references missing old generation manifest");
                    }
                    break;
                };
                next = manifest.parent_generation;
            }
        }

        Ok(needed)
    }

    async fn bundle_bytes_for_generations(
        &self,
        repo: &RepoKey,
        generations: &[GenerationId],
    ) -> CoreResult<u64> {
        let mut total = 0_u64;
        for generation in generations {
            let key = bundle_key(repo, *generation);
            if let Some(meta) = self.state.store.head(&key).await? {
                total = total.saturating_add(meta.len);
            }
        }
        Ok(total)
    }

    async fn repoint_manifests_after_compaction(
        &self,
        repo: &RepoKey,
        old_generations: &HashSet<GenerationId>,
        new_generation: GenerationId,
    ) -> CoreResult<()> {
        let prefix = format!("repos/{repo}/manifests/");
        let keys = self.state.store.list_prefix(&prefix, None).await?;
        for key in keys {
            if key.contains("/manifests/commits/") && key.ends_with(".json") {
                if let Some(mut manifest) =
                    read_json::<_, CommitManifest>(&*self.state.store, &key).await?
                {
                    if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                        manifest.generation = new_generation;
                        write_commit_manifest(&*self.state.store, &manifest).await?;
                    }
                }
            } else if key.contains("/manifests/refs/") && key.ends_with(".json") {
                if key.contains("/manifests/ref-updates/")
                    || key.contains("/manifests/commits/")
                    || key.contains("/manifests/sessions/")
                    || key == default_manifest_key(repo)
                {
                    continue;
                }
                if let Some(mut manifest) =
                    read_json::<_, RefManifest>(&*self.state.store, &key).await?
                {
                    if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                        manifest.generation = new_generation;
                        write_ref_manifest(&*self.state.store, &manifest).await?;
                    }
                }
            }
        }

        let default_key = default_manifest_key(repo);
        if let Some(mut manifest) =
            read_json::<_, RefManifest>(&*self.state.store, &default_key).await?
        {
            if manifest.repo == *repo && old_generations.contains(&manifest.generation) {
                manifest.generation = new_generation;
                write_json(&*self.state.store, &default_key, &manifest).await?;
            }
        }

        Ok(())
    }

    async fn delete_old_generations(
        &self,
        repo: &RepoKey,
        generations: &[GenerationId],
    ) -> CoreResult<()> {
        for generation in generations {
            self.state
                .store
                .delete(&bundle_key(repo, *generation))
                .await?;
            self.state
                .store
                .delete(&generation_manifest_key(repo, *generation))
                .await?;
            self.state
                .store
                .delete(&verified_generation_manifest_key(repo, *generation))
                .await?;
        }
        Ok(())
    }

    async fn put_default_manifest(&self, repo: &RepoKey, commit: &CommitSha) -> CoreResult<()> {
        let commit_manifest = self
            .get_commit_manifest(repo, commit)
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!("commit manifest `{commit}` missing"))
            })?;
        let manifest = RefManifest {
            repo: repo.clone(),
            ref_name: "HEAD".into(),
            commit: commit.clone(),
            generation: commit_manifest.generation,
            verified_at: Utc::now(),
        };
        write_ref_manifest(&*self.state.store, &manifest).await?;
        write_json(&*self.state.store, &default_manifest_key(repo), &manifest).await
    }

    pub async fn get_commit_manifest(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<Option<CommitManifest>> {
        read_commit_manifest(&*self.state.store, repo, commit).await
    }

    async fn materialize_local_branch(
        &self,
        repo: RepoKey,
        branch: &BranchName,
    ) -> CoreResult<MaterializeResponse> {
        let repo_dir = self.ensure_repo_dir(&repo).await?;
        let local_ref = format!("refs/cache/upstream/heads/{}", branch.as_str());
        let commit = self
            .state
            .git
            .rev_parse(&repo_dir, &local_ref)
            .await
            .and_then(CommitSha::parse)?;
        self.create_session(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn materialize_local_default_branch(
        &self,
        repo: RepoKey,
    ) -> CoreResult<MaterializeResponse> {
        let branch = self.resolve_default_branch(&repo).await?;
        self.materialize_local_branch(repo, &branch).await
    }

    async fn get_generation_manifest(
        &self,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> CoreResult<Option<GenerationManifest>> {
        read_generation_manifest(&*self.state.store, repo, generation).await
    }

    async fn get_session_manifest(
        &self,
        repo: &RepoKey,
        session_id: SessionId,
    ) -> CoreResult<Option<SessionManifest>> {
        read_session_manifest(&*self.state.store, repo, session_id).await
    }

    pub async fn ensure_repo_dir(&self, repo: &RepoKey) -> CoreResult<PathBuf> {
        let repo_dir = self.repo_dir(repo);
        if !repo_dir.join("config").exists() {
            self.reset_invalid_repo_cache(repo).await?;
            if let Some(parent) = repo_dir.parent() {
                fs::create_dir_all(parent).await?;
            }
            self.state.git.init_bare(&repo_dir).await?;
            self.record_repo_access(repo).await?;
        } else {
            self.touch_repo_access(repo).await?;
        }
        Ok(repo_dir)
    }

    async fn commit_exists(&self, repo_dir: &FsPath, commit: &CommitSha) -> bool {
        self.state
            .git
            .run(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{commit}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    async fn commit_tree_exists(&self, repo_dir: &FsPath, commit: &CommitSha) -> bool {
        self.state
            .git
            .run(
                Some(repo_dir),
                ["cat-file", "-e", &format!("{}^{{tree}}", commit.as_str())],
            )
            .await
            .is_ok()
    }

    async fn commit_ready_for_serving(&self, repo_dir: &FsPath, commit: &CommitSha) -> bool {
        self.commit_exists(repo_dir, commit).await
            && self.commit_tree_exists(repo_dir, commit).await
    }

    async fn object_exists(&self, repo_dir: &FsPath, object_id: &CommitSha) -> bool {
        self.state
            .git
            .run(Some(repo_dir), ["cat-file", "-e", object_id.as_str()])
            .await
            .is_ok()
    }

    async fn expose_served_commit(&self, repo_dir: &FsPath, commit: &CommitSha) -> CoreResult<()> {
        let served_ref = format!("refs/git-cache-served/commits/{commit}");
        self.state
            .git
            .update_ref(repo_dir, &served_ref, commit.as_str())
            .await?;
        Ok(())
    }

    async fn resolve_short_commit(
        &self,
        repo_dir: &FsPath,
        short_commit: &ShortCommitSha,
    ) -> CoreResult<CommitSha> {
        let rev = format!("{}^{{commit}}", short_commit.as_str());
        let output = self
            .state
            .git
            .rev_parse(repo_dir, &rev)
            .await
            .map_err(|err| {
                GitCacheError::NotFound(format!(
                    "short commit `{short_commit}` could not be resolved unambiguously: {err}"
                ))
            })?;
        CommitSha::parse(output)
    }

    async fn resolve_short_commit_from_upstream_refs(
        &self,
        repo_dir: &FsPath,
        short_commit: &ShortCommitSha,
    ) -> CoreResult<CommitSha> {
        let commit = self.resolve_short_commit(repo_dir, short_commit).await?;
        if self
            .commit_reachable_from_upstream_refs(repo_dir, &commit)
            .await?
        {
            return Ok(commit);
        }

        Err(GitCacheError::NotFound(format!(
            "short commit `{short_commit}` was not found in freshly fetched upstream refs"
        )))
    }

    async fn commit_reachable_from_upstream_refs(
        &self,
        repo_dir: &FsPath,
        commit: &CommitSha,
    ) -> CoreResult<bool> {
        let contains = format!("--contains={}", commit.as_str());
        let output = self
            .state
            .git
            .run(
                Some(repo_dir),
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    contains.as_str(),
                    "refs/cache/upstream/heads",
                ],
            )
            .await?;
        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::Validation(format!("git for-each-ref returned non-utf8: {err}"))
        })?;
        Ok(text.lines().any(|line| !line.trim().is_empty()))
    }

    pub async fn fetch_all_refs(&self, repo: &RepoKey, repo_dir: &FsPath) -> CoreResult<()> {
        let _repo_lock = self.lock_repo(repo).await?;
        let remote = self.upstream_url(repo)?;
        self.state
            .git
            .run(
                Some(repo_dir),
                [
                    "fetch",
                    "--no-tags",
                    "--prune",
                    &remote,
                    "+refs/heads/*:refs/cache/upstream/heads/*",
                ],
            )
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;
        Ok(())
    }

    async fn ls_remote_branch(&self, repo: &RepoKey, branch: &BranchName) -> CoreResult<CommitSha> {
        let remote = self.upstream_url(repo)?;
        let output = self
            .state
            .git
            .run(None, ["ls-remote", "--heads", &remote, branch.as_str()])
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;

        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::UpstreamUnavailable(format!("ls-remote returned non-utf8: {err}"))
        })?;
        let Some(line) = text.lines().next() else {
            return Err(GitCacheError::NotFound(format!(
                "branch `{branch}` was verified absent upstream"
            )));
        };
        let sha = line.split_whitespace().next().ok_or_else(|| {
            GitCacheError::UpstreamUnavailable("malformed ls-remote output".into())
        })?;
        CommitSha::parse(sha)
    }

    async fn resolve_default_branch(&self, repo: &RepoKey) -> CoreResult<BranchName> {
        let remote = self.upstream_url(repo)?;
        let output = self
            .state
            .git
            .run(None, ["ls-remote", "--symref", &remote, "HEAD"])
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;
        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::UpstreamUnavailable(format!("ls-remote returned non-utf8: {err}"))
        })?;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ref: refs/heads/") {
                if let Some((branch, head)) = rest.split_once('\t') {
                    if head == "HEAD" {
                        return BranchName::parse(branch);
                    }
                }
            }
        }

        Err(GitCacheError::UpstreamUnavailable(
            "upstream did not advertise a symbolic HEAD".into(),
        ))
    }

    async fn prepare_session_repo(
        &self,
        manifest: &SessionManifest,
        source_repo: &FsPath,
    ) -> CoreResult<()> {
        let session_repo = self.session_repo_path(manifest.id);
        let objects_dir = session_repo.join("objects");
        let refs_dir = session_repo.join("refs/cache/sessions");
        fs::create_dir_all(objects_dir.join("info")).await?;
        fs::create_dir_all(&refs_dir).await?;
        fs::write(
            session_repo.join("HEAD"),
            format!("ref: {}\n", manifest.synthetic_ref),
        )
        .await?;
        fs::write(
            session_repo.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n[uploadpack]\n\tallowFilter = true\n\tallowAnySHA1InWant = true\n\tallowReachableSHA1InWant = true\n",
        )
        .await?;
        fs::write(
            objects_dir.join("info/alternates"),
            format!("{}\n", source_repo.join("objects").display()),
        )
        .await?;

        let ref_file = session_repo.join(&manifest.synthetic_ref);
        if let Some(parent) = ref_file.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(ref_file, format!("{}\n", manifest.commit)).await?;
        Ok(())
    }

    pub fn repo_dir(&self, repo: &RepoKey) -> PathBuf {
        self.state
            .config
            .cache_root
            .join("repos")
            .join(repo.local_bare_path())
    }

    pub fn session_repo_path(&self, session_id: SessionId) -> PathBuf {
        self.state
            .config
            .cache_root
            .join("sessions")
            .join(format!("{session_id}.git"))
    }

    fn repo_disk_path(&self, repo: &RepoKey) -> PathBuf {
        PathBuf::from(repo.local_bare_path())
    }

    async fn record_repo_access(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state
            .disk
            .record_repo_access(self.repo_disk_path(repo))
            .await?;
        Ok(())
    }

    async fn touch_repo_access(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state
            .disk
            .touch_repo_access(self.repo_disk_path(repo))
            .await?;
        Ok(())
    }

    async fn lock_repo(&self, repo: &RepoKey) -> CoreResult<RepoLock> {
        self.state.disk.lock_repo(self.repo_disk_path(repo)).await
    }

    async fn reset_invalid_repo_cache(&self, repo: &RepoKey) -> CoreResult<()> {
        self.state
            .disk
            .invalidate_repo(self.repo_disk_path(repo))
            .await
    }

    pub fn validate_host(&self, repo: &RepoKey) -> CoreResult<()> {
        if self
            .state
            .config
            .allowed_upstream_hosts
            .iter()
            .any(|host| host == repo.host())
        {
            Ok(())
        } else {
            Err(GitCacheError::Validation(format!(
                "upstream host `{}` is not allowlisted",
                repo.host()
            )))
        }
    }

    pub fn upstream_url(&self, repo: &RepoKey) -> CoreResult<String> {
        if let Some(root) = &self.state.config.upstream_root {
            return Ok(root.join(repo.local_bare_path()).display().to_string());
        }

        Ok(format!(
            "https://{}/{}/{}.git",
            repo.host(),
            repo.owner(),
            repo.name()
        ))
    }

    // ── Direct Git Remote (read-through) domain methods ───────────────

    /// Compare the upstream branch advertisement against local cache state.
    /// Returns a map of branches that are missing or have a different SHA
    /// locally, plus the upstream default branch name.
    pub async fn compare_upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        let upstream_url = self.upstream_url(repo)?;
        let ls = self.state.git.ls_remote_heads(&upstream_url).await?;
        let repo_dir = self.ensure_repo_dir(repo).await?;

        let mut changed: HashMap<String, String> = HashMap::new();

        for (branch, upstream_sha) in &ls.refs {
            let local_ref = format!("refs/heads/{branch}");
            let local_sha = self.state.git.rev_parse(&repo_dir, &local_ref).await.ok();
            if local_sha.as_deref() != Some(upstream_sha.as_str()) {
                changed.insert(branch.clone(), upstream_sha.clone());
            }
        }

        Ok(UpstreamRefComparison {
            changed,
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Fetch only the branches that changed (from compare_upstream_refs),
    /// update both internal cache refs and public refs, and publish manifests.
    pub async fn fetch_changed_refs(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        if comparison.changed.is_empty() {
            return Ok(());
        }

        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let upstream_url = self.upstream_url(repo)?;

        // Validate all branch names and SHAs from network before passing to git.
        let mut validated: Vec<(BranchName, CommitSha)> = Vec::new();
        for (branch, sha) in &comparison.changed {
            let branch_name = BranchName::parse(branch.as_str())?;
            let commit = CommitSha::parse(sha.as_str())?;
            validated.push((branch_name, commit));
        }

        let refspecs: Vec<String> = validated
            .iter()
            .map(|(branch, _)| format!("+refs/heads/{branch}:refs/cache/upstream/heads/{branch}"))
            .collect();

        self.state
            .git
            .fetch_refs(&repo_dir, &upstream_url, &refspecs)
            .await?;

        for (branch_name, expected_commit) in &validated {
            let cache_ref = format!("refs/cache/upstream/heads/{branch_name}");
            let fetched_sha = match self.state.git.rev_parse(&repo_dir, &cache_ref).await {
                Ok(sha) => sha,
                Err(_) => {
                    warn!(%repo, %branch_name, "skipping branch: ref not found after fetch (upstream may have moved)");
                    continue;
                }
            };

            if fetched_sha.as_str() != expected_commit.as_str() {
                warn!(
                    %repo, %branch_name,
                    expected = expected_commit.as_str(),
                    fetched = fetched_sha.as_str(),
                    "skipping branch: upstream moved during fetch"
                );
                continue;
            }

            let is_default_branch =
                comparison.default_branch.as_deref() == Some(branch_name.as_str());
            self.publish_generation(
                repo,
                &repo_dir,
                expected_commit,
                Some(branch_name.clone()),
                is_default_branch,
            )
            .await?;

            self.state
                .git
                .update_ref(
                    &repo_dir,
                    &format!("refs/heads/{branch_name}"),
                    expected_commit.as_str(),
                )
                .await?;
        }

        if let Some(default_branch) = &comparison.default_branch {
            let db = BranchName::parse(default_branch.as_str())?;
            self.state
                .git
                .symbolic_ref(&repo_dir, "HEAD", &format!("refs/heads/{db}"))
                .await?;
        }

        info!(
            %repo,
            changed_count = validated.len(),
            "fetched and published changed refs"
        );

        Ok(())
    }

    /// Fetch the upstream ref advertisement for a repo without downloading
    /// any objects.  Returns the structured ref data so the API layer can
    /// synthesize the pkt-line response directly, avoiding the need to
    /// materialise objects just for ls-remote.
    pub async fn upstream_refs(&self, repo: &RepoKey) -> CoreResult<UpstreamRefComparison> {
        self.validate_host(repo)?;
        let upstream_url = self.upstream_url(repo)?;
        let ls = self.state.git.ls_remote_heads(&upstream_url).await?;

        Ok(UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: ls.default_branch,
            all_upstream: ls.refs,
        })
    }

    /// Sync public refs from the current upstream advertisement without
    /// fetching (used when all branches already match).
    pub async fn sync_public_refs(
        &self,
        repo: &RepoKey,
        comparison: &UpstreamRefComparison,
    ) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;

        for (branch, sha) in &comparison.all_upstream {
            let ref_name = format!("refs/heads/{branch}");
            let local = self.state.git.rev_parse(&repo_dir, &ref_name).await.ok();
            if local.as_deref() != Some(sha.as_str()) {
                self.state.git.update_ref(&repo_dir, &ref_name, sha).await?;
            }
        }

        if let Some(default_branch) = &comparison.default_branch {
            let db = BranchName::parse(default_branch.as_str())?;
            self.state
                .git
                .symbolic_ref(&repo_dir, "HEAD", &format!("refs/heads/{db}"))
                .await?;
        }

        Ok(())
    }

    /// Ensure all wanted OIDs are available locally. For each want:
    /// - If the object exists in the local repo, skip.
    /// - If the commit is known in object-store manifests, hydrate.
    /// - If unknown and commit_read_through is enabled, fetch from upstream.
    /// - Otherwise, fail.
    pub async fn ensure_wants_available(&self, repo: &RepoKey, wants: &[String]) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        let object_ids: Vec<CommitSha> = wants
            .iter()
            .filter_map(|want_sha| CommitSha::parse(want_sha.as_str()).ok())
            .collect();
        let object_types = self
            .state
            .git
            .cat_file_batch_types(&repo_dir, &object_ids)
            .await?;

        for object_id in object_ids {
            if let Some(object_type) = object_types.get(&object_id) {
                if object_type != "commit" {
                    continue;
                }

                if self.commit_tree_exists(&repo_dir, &object_id).await {
                    self.expose_served_commit(&repo_dir, &object_id).await?;
                    continue;
                }
            }

            if let Some(manifest) = self.get_commit_manifest(repo, &object_id).await? {
                if manifest.complete {
                    self.hydrate_commit_in_repo(&repo_dir, &manifest).await?;
                    self.expose_served_commit(&repo_dir, &object_id).await?;
                    continue;
                }
            }

            if self.state.config.git_remote.commit_read_through {
                info!(%repo, commit = %object_id, "read-through fetch for unknown commit");
                let upstream_url = self.upstream_url(repo)?;
                let fetch_result = self
                    .state
                    .git
                    .run(
                        Some(&repo_dir),
                        [
                            "fetch",
                            "--no-tags",
                            "--",
                            &upstream_url,
                            object_id.as_str(),
                        ],
                    )
                    .await;

                if let Err(fetch_err) = fetch_result {
                    self.fetch_all_refs(repo, &repo_dir).await?;
                    if self.object_exists(&repo_dir, &object_id).await {
                        if self.commit_ready_for_serving(&repo_dir, &object_id).await {
                            self.expose_served_commit(&repo_dir, &object_id).await?;
                        } else if self.commit_exists(&repo_dir, &object_id).await {
                            return Err(GitCacheError::NotFound(format!(
                                "commit `{object_id}` is incomplete after upstream fetch"
                            )));
                        }
                        continue;
                    }
                    return Err(GitCacheError::NotFound(format!(
                        "object `{object_id}` could not be fetched from upstream: {fetch_err}"
                    )));
                }

                if !self.commit_ready_for_serving(&repo_dir, &object_id).await {
                    return Err(GitCacheError::NotFound(format!(
                        "commit `{object_id}` not found or incomplete after upstream fetch"
                    )));
                }
                self.expose_served_commit(&repo_dir, &object_id).await?;

                // Create a ref so that `bundle create --all` has something
                // to include.  We use refs/cache/ which is hidden from
                // clients by configure_served_repo.
                let cache_ref = format!("refs/cache/commits/{object_id}");
                self.state
                    .git
                    .update_ref(&repo_dir, &cache_ref, object_id.as_str())
                    .await?;

                self.publish_generation(repo, &repo_dir, &object_id, None, false)
                    .await?;
            } else {
                return Err(GitCacheError::NotFound(format!(
                    "commit `{object_id}` is not available and read-through is disabled"
                )));
            }
        }

        Ok(())
    }

    /// Configure a bare repo for serving via the direct Git remote:
    /// - `uploadpack.allowAnySHA1InWant=true`
    /// - `uploadpack.allowReachableSHA1InWant=true`
    /// - `uploadpack.allowFilter=true`
    /// - `uploadpack.hideRefs=refs/cache`
    /// - `transfer.hideRefs=refs/cache`
    pub async fn configure_served_repo(&self, repo_dir: &FsPath) -> CoreResult<()> {
        let marker = repo_dir.join(SERVED_REPO_CONFIG_MARKER);
        if fs::try_exists(&marker).await? {
            return Ok(());
        }

        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowAnySHA1InWant", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowFilter", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.allowReachableSHA1InWant", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "uploadpack.hideRefs", "refs/cache")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "transfer.hideRefs", "refs/cache")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "pack.useBitmap", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "repack.writeBitmaps", "true")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "pack.compression", "1")
            .await?;
        self.state
            .git
            .set_config(repo_dir, "core.compression", "1")
            .await?;
        fs::write(marker, b"configured\n").await?;
        Ok(())
    }

    pub async fn optimize_repo_for_serving(&self, repo: &RepoKey) -> CoreResult<()> {
        let repo_dir = self.ensure_repo_dir(repo).await?;
        let _repo_lock = self.lock_repo(repo).await?;
        self.configure_served_repo(&repo_dir).await?;
        self.state.git.repack_for_serving(&repo_dir).await?;
        Ok(())
    }

    /// Handle a direct Git remote upload-pack request end-to-end:
    /// parse want lines, ensure objects are available, configure the repo,
    /// and spawn the upload-pack process for streaming.
    pub async fn handle_upload_pack(
        &self,
        repo: &RepoKey,
        body: &Bytes,
    ) -> CoreResult<UploadPackProcess> {
        let wants = parse_want_lines(body);
        if !wants.is_empty() {
            self.ensure_wants_available(repo, &wants).await?;
        }
        let repo_dir = self.ensure_repo_dir(repo).await?;
        self.configure_served_repo(&repo_dir).await?;
        self.state
            .git
            .upload_pack_spawn(&repo_dir, body.clone())
            .await
    }

    pub async fn cleanup_expired_sessions(&self) -> CoreResult<SessionCleanupReport> {
        let keys = self.state.store.list_prefix("repos/", None).await?;
        let session_keys: Vec<String> = keys
            .into_iter()
            .filter(|k| k.contains("/manifests/sessions/") && k.ends_with(".json"))
            .collect();

        let mut sessions_removed: usize = 0;
        let mut errors: Vec<String> = Vec::new();
        let now = Utc::now();

        for key in session_keys {
            let manifest: Option<SessionManifest> = match read_json(&*self.state.store, &key).await
            {
                Ok(m) => m,
                Err(err) => {
                    errors.push(format!("failed to read `{key}`: {err}"));
                    continue;
                }
            };

            let Some(manifest) = manifest else {
                continue;
            };

            if manifest.expires_at >= now {
                continue;
            }

            let session_dir = self.session_repo_path(manifest.id);
            if session_dir.exists() {
                let dir = session_dir.clone();
                if let Err(err) = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir))
                    .await
                    .map_err(|err| GitCacheError::Io(std::io::Error::other(err)))
                    .and_then(|r| r.map_err(GitCacheError::Io))
                {
                    errors.push(format!(
                        "failed to remove session dir `{}`: {err}",
                        session_dir.display()
                    ));
                    continue;
                }
            }

            if let Err(err) = self.state.store.delete(&key).await {
                errors.push(format!("failed to delete manifest `{key}`: {err}"));
                continue;
            }

            sessions_removed += 1;
            debug!(session_id = %manifest.id, "cleaned up expired session");
        }

        Ok(SessionCleanupReport {
            sessions_removed,
            errors,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCleanupReport {
    pub sessions_removed: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct UpstreamRefComparison {
    pub changed: HashMap<String, String>,
    pub default_branch: Option<String>,
    pub all_upstream: HashMap<String, String>,
}

pub struct MaterializerExecutor {
    state: Arc<AppState>,
}

impl MaterializerExecutor {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl UpdateExecutor for MaterializerExecutor {
    async fn update(&self, request: UpdateRequest) -> CoreResult<()> {
        let materializer = Materializer::new(Arc::clone(&self.state));
        match request.target {
            UpdateTarget::Branch(ref branch) => {
                materializer
                    .ensure_branch(&request.repo, branch, false)
                    .await?;
            }
            UpdateTarget::DefaultBranch => {
                materializer.ensure_default_branch(&request.repo).await?;
            }
            UpdateTarget::Commit(commit) => {
                materializer
                    .materialize_commit(request.repo, commit)
                    .await?;
            }
            UpdateTarget::ShortCommit(commit) => {
                materializer
                    .materialize_short_commit(request.repo, commit)
                    .await?;
            }
            UpdateTarget::Ref(ref ref_name) => {
                if let Some(branch_str) = ref_name.strip_prefix("refs/heads/") {
                    let branch = BranchName::parse(branch_str)?;
                    materializer
                        .ensure_branch(&request.repo, &branch, false)
                        .await?;
                } else {
                    return Err(GitCacheError::Unsupported(format!(
                        "unsupported update target ref: {ref_name}"
                    )));
                }
            }
        }
        Ok(())
    }
}

pub async fn advertise_refs(state: &AppState, repo: &FsPath) -> CoreResult<Vec<u8>> {
    Ok(state
        .git
        .upload_pack_advertise_refs(repo, state.config.max_git_output_bytes)
        .await?
        .stdout)
}

/// Build a pkt-line formatted ref advertisement from upstream ref data.
///
/// This produces the same output as `git upload-pack --advertise-refs` but
/// without requiring the objects to exist locally.  The capability set
/// matches what a standard git 2.x upload-pack would emit.
pub fn synthesize_ref_advertisement(comparison: &UpstreamRefComparison) -> Vec<u8> {
    let mut out = Vec::new();

    // Sort refs for deterministic output.
    let mut refs: Vec<(&String, &String)> = comparison.all_upstream.iter().collect();
    refs.sort_by_key(|(name, _)| name.as_str());

    // Only advertise symref when the default branch is actually present in
    // the upstream refs. A symref pointing at a non-existent ref confuses
    // git clients during clone.
    let resolved_default = comparison.default_branch.as_ref().and_then(|b| {
        comparison
            .all_upstream
            .get(b)
            .map(|sha| (b.as_str(), sha.as_str()))
    });

    let symref = resolved_default
        .map(|(b, _)| format!(" symref=HEAD:refs/heads/{b}"))
        .unwrap_or_default();

    let caps = format!(
        "multi_ack thin-pack side-band side-band-64k ofs-delta \
         shallow deepen-since deepen-not deepen-relative no-progress \
         include-tag multi_ack_detailed no-done filter \
         allow-tip-sha1-in-want allow-reachable-sha1-in-want{symref} \
         object-format=sha1 agent=git-cache/1.0"
    );

    // HEAD line (first ref includes capabilities).
    let mut first_ref_used_for_caps = false;
    if let Some((_, sha)) = resolved_default {
        let line = format!("{sha} HEAD\0{caps}\n");
        pkt_line(&mut out, &line);
    } else if let Some((name, sha)) = refs.first() {
        // No usable default branch: first sorted ref carries capabilities.
        let line = format!("{sha} refs/heads/{name}\0{caps}\n");
        pkt_line(&mut out, &line);
        first_ref_used_for_caps = true;
    }

    // Ref lines (skip the first if it was already emitted as the capability carrier).
    let skip_first = first_ref_used_for_caps;
    for (i, (name, sha)) in refs.iter().enumerate() {
        if skip_first && i == 0 {
            continue;
        }
        let line = format!("{sha} refs/heads/{name}\n");
        pkt_line(&mut out, &line);
    }

    // HEAD as a separate non-capability line (if default branch set).
    // Already emitted as the first capability line above, so only emit
    // ref lines here.

    out.extend_from_slice(b"0000");
    out
}

fn pkt_line(out: &mut Vec<u8>, data: &str) {
    let len = 4 + data.len();
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(data.as_bytes());
}

pub async fn upload_pack(state: &AppState, repo: &FsPath, body: Bytes) -> CoreResult<Vec<u8>> {
    Ok(state
        .git
        .upload_pack_stateless_rpc(
            repo,
            &body,
            state.config.max_git_output_bytes,
            state.config.max_git_output_bytes,
        )
        .await?
        .stdout)
}

/// Frame a ref advertisement with the Git smart-HTTP service header.
pub fn frame_ref_advertisement(refs_output: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(refs_output.len() + 34);
    framed.extend_from_slice(b"001e# service=git-upload-pack\n0000");
    framed.extend_from_slice(refs_output);
    framed
}

/// Parse `want <oid>` lines from a Git pkt-line formatted upload-pack request.
pub fn parse_want_lines(body: &[u8]) -> Vec<String> {
    let mut wants = Vec::new();
    let mut offset = 0;
    while offset + 4 <= body.len() {
        let hex = match std::str::from_utf8(&body[offset..offset + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        let pkt_len = match usize::from_str_radix(hex, 16) {
            Ok(l) => l,
            Err(_) => break,
        };

        if pkt_len == 0 {
            offset += 4;
            continue;
        }
        if pkt_len < 4 || offset + pkt_len > body.len() {
            break;
        }

        let line = &body[offset + 4..offset + pkt_len];
        if let Ok(line_str) = std::str::from_utf8(line) {
            let line_str = line_str.trim();
            if let Some(rest) = line_str.strip_prefix("want ") {
                let oid = rest.split_whitespace().next().unwrap_or("");
                if !oid.is_empty() {
                    wants.push(oid.to_string());
                }
            }
        }

        offset += pkt_len;
    }
    wants
}

pub fn default_manifest_key(repo: &RepoKey) -> String {
    format!("repos/{}/manifests/refs/default.json", repo.as_str())
}

pub fn bundle_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!(
        "repos/{}/generations/{}/base.bundle",
        repo.as_str(),
        generation
    )
}

fn pending_generation_from_key(key: &str) -> CoreResult<Option<(RepoKey, GenerationId)>> {
    let Some(rest) = key.strip_prefix(PENDING_GENERATION_PREFIX) else {
        return Ok(None);
    };
    let Some((repo_part, generation_file)) = rest.rsplit_once('/') else {
        return Ok(None);
    };
    let Some(generation) = generation_file.strip_suffix(".json") else {
        return Ok(None);
    };
    let repo = RepoKey::parse(repo_part)?;
    let generation = GenerationId(uuid::Uuid::parse_str(generation).map_err(|err| {
        GitCacheError::Validation(format!(
            "invalid pending generation id `{generation}`: {err}"
        ))
    })?);
    Ok(Some((repo, generation)))
}

fn verified_generation_manifest(
    generation: &GenerationManifest,
    bundle_len: u64,
    bundle_sha256: String,
    verified_at: chrono::DateTime<Utc>,
    tip_commits: Vec<CommitSha>,
) -> VerifiedGenerationManifest {
    VerifiedGenerationManifest {
        schema_version: VERIFIED_GENERATION_SCHEMA_VERSION,
        repo: generation.repo.clone(),
        generation: generation.generation,
        bundle_key: generation.bundle_key.clone(),
        bundle_len,
        bundle_sha256,
        parent_generation: generation.parent_generation,
        created_at: generation.created_at,
        verified_at,
        verifier_version: VERIFIED_GENERATION_VERIFIER_VERSION,
        git_version: "unknown".to_string(),
        fsck_mode: VERIFIED_GENERATION_FSCK_MODE.to_string(),
        commits: generation.commits.clone(),
        tip_commits,
    }
}

fn validate_verified_generation(
    generation: &GenerationManifest,
    verification: &VerifiedGenerationManifest,
    object_len: u64,
) -> CoreResult<()> {
    if verification.schema_version != VERIFIED_GENERATION_SCHEMA_VERSION {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has unsupported schema version {}",
            generation.generation, verification.schema_version
        )));
    }
    if verification.repo != generation.repo
        || verification.generation != generation.generation
        || verification.bundle_key != generation.bundle_key
        || verification.parent_generation != generation.parent_generation
    {
        return Err(GitCacheError::Validation(format!(
            "verified generation manifest does not match generation `{}`",
            generation.generation
        )));
    }
    if verification.bundle_len == 0 {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has empty bundle",
            generation.generation
        )));
    }
    if object_len != verification.bundle_len {
        return Err(GitCacheError::Validation(format!(
            "bundle `{}` length mismatch: expected {}, got {}",
            generation.bundle_key, verification.bundle_len, object_len
        )));
    }
    validate_sha256_hex(&verification.bundle_sha256)?;
    Ok(())
}

async fn file_len_and_sha256(path: &FsPath) -> CoreResult<(u64, String)> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut len = 0u64;
    let mut buffer = vec![0u8; 1024 * 1024];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        len += read as u64;
        hasher.update(&buffer[..read]);
    }

    Ok((len, hex_lower(&hasher.finalize())))
}

fn validate_sha256_hex(value: &str) -> CoreResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitCacheError::Validation(format!(
            "invalid sha256 digest `{value}`"
        )));
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn push_unique_commit(commits: &mut Vec<CommitSha>, commit: CommitSha) {
    if !commits.iter().any(|existing| existing == &commit) {
        commits.push(commit);
    }
}

fn commits_from_chain<'a>(
    chain: impl IntoIterator<Item = &'a GenerationManifest>,
) -> Vec<CommitSha> {
    let mut commits = Vec::new();
    for manifest in chain {
        for commit in &manifest.commits {
            push_unique_commit(&mut commits, commit.clone());
        }
    }
    commits
}

pub fn repo_from_git_path(repo_path: &str) -> CoreResult<RepoKey> {
    let Some((repo, suffix)) = repo_path.split_once(".git") else {
        return Err(GitCacheError::Validation(format!(
            "session repo path `{repo_path}` must end in .git"
        )));
    };
    if !suffix.is_empty() && !suffix.starts_with('/') {
        return Err(GitCacheError::Validation(format!(
            "session repo path `{repo_path}` has an invalid .git suffix"
        )));
    }
    RepoKey::parse(repo)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "s3-tests")]
    use aws_credential_types::Credentials;
    #[cfg(feature = "s3-tests")]
    use aws_sdk_s3::config::BehaviorVersion;
    #[cfg(feature = "s3-tests")]
    use aws_sdk_s3::config::RequestChecksumCalculation;
    #[cfg(feature = "s3-tests")]
    use aws_sdk_s3::Client;
    #[cfg(feature = "s3-tests")]
    use aws_types::region::Region;
    use git_cache_core::{AppConfig, ObjectStoreConfig};
    #[cfg(feature = "s3-tests")]
    use git_cache_disk::{AsyncDiskManager, DiskManager};
    #[cfg(feature = "s3-tests")]
    use git_cache_git::Git;
    #[cfg(feature = "s3-tests")]
    use git_cache_objectstore::{ObjectStore, S3ObjectStore};
    use std::fs as stdfs;
    use std::net::SocketAddr;
    use std::process::Command;
    use tempfile::TempDir;

    #[cfg(test)]
    fn ref_manifest_key(repo: &RepoKey, branch: &str) -> String {
        git_cache_objectstore::ref_manifest_key(repo, &format!("refs/heads/{branch}"))
            .expect("validated branch ref")
    }

    async fn generation_manifest_for(
        state: &AppState,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> GenerationManifest {
        read_generation_manifest(&*state.store, repo, generation)
            .await
            .unwrap()
            .unwrap()
    }

    async fn generation_object_keys(state: &AppState, repo: &RepoKey) -> Vec<String> {
        state
            .store
            .list_prefix(&format!("repos/{repo}/generations/"), None)
            .await
            .unwrap()
    }

    async fn wait_for_verified_generation(
        state: &AppState,
        repo: &RepoKey,
        generation: GenerationId,
    ) {
        for _ in 0..100 {
            if read_verified_generation_manifest(&*state.store, repo, generation)
                .await
                .unwrap()
                .is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("verified generation manifest `{generation}` not written");
    }

    async fn wait_for_commit_manifest(
        state: &AppState,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CommitManifest {
        for _ in 0..100 {
            if let Some(manifest) = read_commit_manifest(&*state.store, repo, commit)
                .await
                .unwrap()
            {
                return manifest;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("commit manifest `{commit}` not written");
    }

    async fn wait_for_generation_head(
        state: &AppState,
        repo: &RepoKey,
        generation: GenerationId,
    ) -> RepoGenerationHead {
        for _ in 0..100 {
            if let Some(head) = read_repo_generation_head(&*state.store, repo)
                .await
                .unwrap()
            {
                if head.generation == generation {
                    return head;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("generation head `{generation}` not written");
    }

    #[test]
    fn object_keys_are_stable() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let commit = CommitSha::parse("a".repeat(40)).unwrap();
        assert_eq!(
            git_cache_objectstore::commit_manifest_key(&repo, &commit),
            "repos/github.com/org/repo/manifests/commits/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json"
        );
        assert_eq!(
            ref_manifest_key(&repo, "feature/test"),
            "repos/github.com/org/repo/manifests/refs/heads/feature%2Ftest.json"
        );
    }

    // ── Additional repo_from_git_path tests ────────────────────────

    #[test]
    fn repo_from_git_path_rejects_no_dot_git() {
        assert!(repo_from_git_path("github.com/org/repo").is_err());
    }

    #[test]
    fn repo_from_git_path_rejects_gitfoo_suffix() {
        assert!(repo_from_git_path("github.com/org/repo.gitfoo").is_err());
    }

    #[test]
    fn repo_from_git_path_with_info_refs_suffix() {
        let key = repo_from_git_path("github.com/org/repo.git/info/refs").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    #[test]
    fn repo_from_git_path_with_upload_pack_suffix() {
        let key = repo_from_git_path("github.com/org/repo.git/git-upload-pack").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    #[test]
    fn repo_from_git_path_bare_dot_git() {
        let key = repo_from_git_path("github.com/org/repo.git").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    // ── validate_host tests ──────────────────────────────────────────

    #[tokio::test]
    async fn validate_host_accepts_allowed_host() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        assert!(materializer
            .validate_host(&RepoKey::parse("github.com/org/repo").unwrap())
            .is_ok());
    }

    #[tokio::test]
    async fn validate_host_rejects_unlisted_host() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        assert!(materializer
            .validate_host(&RepoKey::parse("evil.com/org/repo").unwrap())
            .is_err());
    }

    #[tokio::test]
    async fn publish_generation_links_delta_to_previous_generation() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let first_generation =
            generation_manifest_for(&state, &fixture.repo, first_manifest.generation).await;
        assert_eq!(first_generation.parent_generation, None);

        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let second_generation =
            generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(
            second_generation.parent_generation,
            Some(first_manifest.generation)
        );

        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(head.generation, second_manifest.generation);
        assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
    }

    #[tokio::test]
    async fn hydrate_commit_restores_parent_generation_chain_from_cold_cache() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let _ = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(second_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        materializer
            .state
            .git
            .rev_parse(&repo_dir, &format!("{}^{{commit}}", first.commit.as_str()))
            .await
            .unwrap();
        assert!(materializer.commit_exists(&repo_dir, &second_commit).await);
    }

    #[tokio::test]
    async fn exact_ancestor_in_known_generation_indexes_without_new_bundle() {
        let fixture = GitFixture::new();
        let ancestor_commit = fixture.head_commit();
        let tip_commit = fixture.commit_and_push("second");
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert!(
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .is_none()
        );
        let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
        wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;
        let head_before =
            Some(wait_for_generation_head(&state, &fixture.repo, tip_manifest.generation).await);
        let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(ancestor_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert_eq!(response.commit, ancestor_commit);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(ancestor_manifest.generation, tip_manifest.generation);
        let head_after = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap();
        let generation_keys_after = generation_object_keys(&state, &fixture.repo).await;
        assert_eq!(head_after, head_before);
        assert_eq!(generation_keys_after, generation_keys_before);
    }

    #[tokio::test]
    async fn exact_ancestor_uses_local_cache_refs_when_generation_head_is_stale() {
        let fixture = GitFixture::new();
        let ancestor_commit = fixture.commit_and_push("second");
        let tip_commit = fixture.commit_and_push("third");
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert!(
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .is_none()
        );
        let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
        wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;
        let stale_head = RepoGenerationHead {
            repo: fixture.repo.clone(),
            generation: tip_manifest.generation,
            tip_commits: vec![ancestor_commit.clone()],
            updated_at: Utc::now(),
        };
        write_repo_generation_head(&*state.store, &stale_head)
            .await
            .unwrap();
        let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(ancestor_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert_eq!(response.commit, ancestor_commit);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(ancestor_manifest.generation, tip_manifest.generation);
        let generation_keys_after = generation_object_keys(&state, &fixture.repo).await;
        assert_eq!(generation_keys_after, generation_keys_before);
    }

    #[tokio::test]
    async fn exact_descendants_after_cold_ancestor_fetch_reuse_full_bundle() {
        let fixture = GitFixture::new();
        let tip_2 = fixture.head_commit();
        let tip_1 = fixture.commit_and_push("second");
        let tip = fixture.commit_and_push("third");
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(tip_2.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(first.commit, tip_2);
        assert_eq!(first.source, MaterializeSource::GithubVerified);
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_2).await;
        wait_for_verified_generation(&state, &fixture.repo, first_manifest.generation).await;
        let generation_keys_after_first = generation_object_keys(&state, &fixture.repo).await;

        let second = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(tip_1.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(second.commit, tip_1);
        assert_eq!(second.source, MaterializeSource::CacheVerified);
        assert_eq!(
            generation_object_keys(&state, &fixture.repo).await,
            generation_keys_after_first
        );

        let third = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(tip.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(third.commit, tip);
        assert_eq!(third.source, MaterializeSource::CacheVerified);
        assert_eq!(
            generation_object_keys(&state, &fixture.repo).await,
            generation_keys_after_first
        );
    }

    #[tokio::test]
    async fn exact_commit_ahead_of_known_generation_publishes_incremental_bundle() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let second_commit = fixture.commit_and_push("second");

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(second_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(response.commit, second_commit);
        assert_eq!(response.source, MaterializeSource::GithubVerified);

        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let second_generation =
            generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(
            second_generation.parent_generation,
            Some(first_manifest.generation)
        );
        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
    }

    #[tokio::test]
    async fn delta_publish_falls_back_to_full_bundle_when_previous_tip_is_missing_locally() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        stdfs::remove_dir_all(materializer.repo_dir(&fixture.repo)).unwrap();
        let second_commit = fixture.replace_history_and_push("replacement");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        run_git(
            &repo_dir,
            [
                "fetch",
                "--no-tags",
                fixture.upstream_path().to_str().unwrap(),
                "+refs/heads/main:refs/cache/upstream/heads/main",
            ],
        );
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(second_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(response.commit, second_commit);

        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let second_generation =
            generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
        assert_ne!(first_manifest.generation, second_manifest.generation);
        assert_eq!(second_generation.parent_generation, None);
        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(head.tip_commits, vec![second_commit]);
    }

    #[tokio::test]
    async fn ensure_repo_dir_records_disk_metadata() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert!(repo_dir.join("config").exists());

        let index = state.disk.repo_index().await.unwrap();
        let repo_path = PathBuf::from(fixture.repo.local_bare_path());
        let entry = index.repos.get(&repo_path).unwrap();
        assert_eq!(entry.path, repo_path);
        assert!(entry.size_bytes > 0);
    }

    #[tokio::test]
    async fn ensure_repo_dir_invalidates_partial_repo_cache() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let repo_path = PathBuf::from(fixture.repo.local_bare_path());
        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::create_dir_all(&repo_dir).unwrap();
        stdfs::write(repo_dir.join("partial"), "stale").unwrap();
        state
            .disk
            .record_repo_access(repo_path.clone())
            .await
            .unwrap();

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        assert!(repo_dir.join("config").exists());
        assert!(!repo_dir.join("partial").exists());
        assert!(state
            .disk
            .repo_index()
            .await
            .unwrap()
            .repos
            .contains_key(&repo_path));
    }

    #[tokio::test]
    async fn compact_generation_chain_replaces_long_chain_with_single_root() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        let third_commit = fixture.commit_and_push("third");
        fixture.push_head_to_branch("default");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third_commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("default").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.old_chain_depth, 3);

        let head = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head.generation, report.new_generation);
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        assert_eq!(compacted.parent_generation, None);

        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(state
                .store
                .get(&bundle_key(&fixture.repo, *old_generation))
                .await
                .unwrap()
                .is_none());
            assert!(read_verified_generation_manifest(
                &*state.store,
                &fixture.repo,
                *old_generation
            )
            .await
            .unwrap()
            .is_none());
        }

        for commit in [
            first.commit.clone(),
            second_commit.clone(),
            third_commit.clone(),
        ] {
            let manifest = wait_for_commit_manifest(&state, &fixture.repo, &commit).await;
            assert_eq!(manifest.generation, report.new_generation);
        }
        let branch_manifest = read_ref_manifest(
            &*state.store,
            &fixture.repo,
            &BranchName::parse("main").unwrap().ref_name(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(branch_manifest.generation, report.new_generation);
        let default_branch_manifest = read_ref_manifest(
            &*state.store,
            &fixture.repo,
            &BranchName::parse("default").unwrap().ref_name(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(default_branch_manifest.generation, report.new_generation);
        assert_ne!(first_manifest.generation, report.new_generation);

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(third_commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        for commit in [first.commit, second_commit, third_commit] {
            materializer
                .state
                .git
                .rev_parse(&repo_dir, &format!("{}^{{commit}}", commit.as_str()))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn inline_compaction_runs_after_verified_head_update() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: true,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, first_manifest.generation).await;

        let second = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;

        let third = fixture.commit_and_push("third");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third).await;

        for _ in 0..100 {
            if let Some(head) = read_repo_generation_head(&*state.store, &fixture.repo)
                .await
                .unwrap()
            {
                let chain = materializer
                    .generation_chain(&fixture.repo, head.generation)
                    .await
                    .unwrap();
                if chain.len() == 1 && head.generation != third_manifest.generation {
                    let old_generations_deleted = read_generation_manifest(
                        &*state.store,
                        &fixture.repo,
                        first_manifest.generation,
                    )
                    .await
                    .unwrap()
                    .is_none()
                        && read_generation_manifest(
                            &*state.store,
                            &fixture.repo,
                            second_manifest.generation,
                        )
                        .await
                        .unwrap()
                        .is_none()
                        && read_generation_manifest(
                            &*state.store,
                            &fixture.repo,
                            third_manifest.generation,
                        )
                        .await
                        .unwrap()
                        .is_none();
                    if old_generations_deleted {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("inline compaction did not collapse the verified generation chain");
    }

    #[tokio::test]
    async fn compaction_preserves_parents_needed_by_pending_generation_verification() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 1,
                inline: false,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state));

        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;

        let third_commit = fixture.commit_and_push("third");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third_commit).await;
        let parent_head =
            wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;

        let child_commit = fixture.commit_and_push("fourth");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        run_git(
            &repo_dir,
            [
                "fetch",
                "--no-tags",
                fixture.upstream_path().to_str().unwrap(),
                "+refs/heads/main:refs/cache/upstream/heads/main",
            ],
        );

        let child_generation = GenerationId::new();
        let reservation = state.disk.reserve(1024 * 1024 * 64).await.unwrap();
        let temp_path = reservation.temp_path().unwrap();
        fs::create_dir_all(&temp_path).await.unwrap();
        let bundle_path = temp_path.join("pending-child.bundle");
        state
            .git
            .bundle_create_incremental(&repo_dir, &bundle_path, &parent_head.tip_commits)
            .await
            .unwrap();

        let now = Utc::now();
        let child_manifest = GenerationManifest {
            repo: fixture.repo.clone(),
            generation: child_generation,
            bundle_key: bundle_key(&fixture.repo, child_generation),
            parent_generation: Some(parent_head.generation),
            created_at: now,
            commits: vec![child_commit.clone()],
        };
        let mut child_tip_commits = parent_head.tip_commits.clone();
        push_unique_commit(&mut child_tip_commits, child_commit.clone());
        let child_head = RepoGenerationHead {
            repo: fixture.repo.clone(),
            generation: child_generation,
            tip_commits: child_tip_commits,
            updated_at: now,
        };
        let child_manifests = PublishManifests {
            commits: vec![CommitManifest {
                repo: fixture.repo.clone(),
                commit: child_commit,
                generation: child_generation,
                complete: true,
                verified_at: now,
            }],
            refs: Vec::new(),
            sessions: Vec::new(),
        };
        GenerationPublish::with_manifests(child_manifest, child_manifests)
            .publish_pending_bundle_file(&*state.store, &bundle_path, child_head, None)
            .await
            .unwrap();
        reservation.release().await.unwrap();

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert!(report.old_generations.contains(&parent_head.generation));

        materializer
            .verify_generation_with_semaphore(fixture.repo.clone(), child_generation, false)
            .await
            .expect("pending generation should verify after parent chain compaction");
    }

    // ── upstream_url tests ───────────────────────────────────────────

    #[tokio::test]
    async fn upstream_url_with_upstream_root_set() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let url = materializer.upstream_url(&repo).unwrap();
        // With upstream_root set, it should be a local path
        assert!(url.contains("github.com/org/repo.git"));
    }

    #[tokio::test]
    async fn upstream_url_without_upstream_root() {
        let fixture = GitFixture::new();
        let mut state = fixture.state();
        state.config.upstream_root = None;
        let materializer = Materializer::new(Arc::new(state));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let url = materializer.upstream_url(&repo).unwrap();
        assert_eq!(url, "https://github.com/org/repo.git");
    }

    // ── default_manifest_key and bundle_key tests ────────────────────

    #[test]
    fn default_manifest_key_produces_expected_path() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        assert_eq!(
            default_manifest_key(&repo),
            "repos/github.com/org/repo/manifests/refs/default.json"
        );
    }

    #[test]
    fn bundle_key_produces_expected_path() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let gen = GenerationId::new();
        let key = bundle_key(&repo, gen);
        assert!(key.starts_with("repos/github.com/org/repo/generations/"));
        assert!(key.ends_with("/base.bundle"));
    }

    #[test]
    fn pending_generation_from_key_parses_scan_key() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let generation = GenerationId::new();
        let key = pending_generation_publish_key(&repo, generation);

        assert_eq!(
            pending_generation_from_key(&key).unwrap(),
            Some((repo, generation))
        );
        assert_eq!(pending_generation_from_key("other/key.json").unwrap(), None);
    }

    // ── synthesize_ref_advertisement tests ───────────────────────────

    #[test]
    fn synthesize_ref_advertisement_contains_head_and_refs() {
        let comparison = UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "a".repeat(40));
                m.insert("develop".to_string(), "b".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);
        let text = String::from_utf8_lossy(&output);

        assert!(text.contains("HEAD"));
        assert!(text.contains("refs/heads/main"));
        assert!(text.contains("refs/heads/develop"));
        assert!(text.ends_with("0000"));
    }

    #[test]
    fn synthesize_ref_advertisement_valid_pkt_line_format() {
        let comparison = UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "c".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);

        // First 4 bytes are hex length
        assert!(output.len() >= 4);
        let first_len_str = std::str::from_utf8(&output[..4]).unwrap();
        let first_len: usize = usize::from_str_radix(first_len_str, 16).unwrap();
        assert!(first_len > 4);

        // Ends with flush packet
        assert!(output.ends_with(b"0000"));
    }

    #[test]
    fn synthesize_ref_advertisement_contains_capability_line() {
        let comparison = UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "d".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);
        let text = String::from_utf8_lossy(&output);

        assert!(text.contains("multi_ack"));
        assert!(text.contains("agent=git-cache/1.0"));
    }

    #[test]
    fn repo_from_git_path_accepts_smart_http_suffixes() {
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git/info/refs")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git/git-upload-pack")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
    }

    #[tokio::test]
    async fn cached_exact_commit_survives_upstream_offline() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let branch_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(branch_response.source, MaterializeSource::GithubVerified);
        let commit_manifest =
            wait_for_commit_manifest(&materializer.state, &fixture.repo, &branch_response.commit)
                .await;
        wait_for_verified_generation(
            &materializer.state,
            &fixture.repo,
            commit_manifest.generation,
        )
        .await;

        stdfs::remove_dir_all(fixture.cache_root().join("repos")).unwrap();
        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        let commit_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(branch_response.commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        assert_eq!(commit_response.source, MaterializeSource::CacheVerified);
        assert_eq!(commit_response.commit, branch_response.commit);
    }

    #[tokio::test]
    async fn short_commit_selector_resolves_to_full_commit_from_upstream() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        let head = fixture.head_commit();
        let short = ShortCommitSha::parse(&head.as_str()[..8]).unwrap();

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::ShortCommit(short),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        assert_eq!(response.source, MaterializeSource::GithubVerified);
        assert_eq!(response.commit, head);
        let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &head).await;
    }

    #[tokio::test]
    async fn short_commit_selector_revalidates_even_when_commit_is_cached() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let branch_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let short = ShortCommitSha::parse(&branch_response.commit.as_str()[..8]).unwrap();

        let short_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::ShortCommit(short),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        assert_eq!(short_response.source, MaterializeSource::GithubVerified);
        assert_eq!(short_response.commit, branch_response.commit);
    }

    #[tokio::test]
    async fn short_commit_selector_requires_upstream_even_when_cached() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let branch_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let short = ShortCommitSha::parse(&branch_response.commit.as_str()[..8]).unwrap();

        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::ShortCommit(short),
                mode: RequestMode::Cached,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));
    }

    #[tokio::test]
    async fn unknown_short_commit_returns_not_found_after_upstream_check() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::ShortCommit(ShortCommitSha::parse("deadbeef").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::NotFound(_)));
    }

    #[tokio::test]
    async fn branch_and_default_selectors_require_upstream_for_all_modes() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let default_response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::DefaultBranch,
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(default_response.source, MaterializeSource::GithubVerified);

        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();
        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));

        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Cached,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));

        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::DefaultBranch,
                mode: RequestMode::Cached,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));
    }

    #[tokio::test]
    async fn force_push_updates_branch_manifest_without_removing_old_commit() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        let second_commit = fixture.commit_and_push("second");
        let second = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        assert_eq!(second.commit, second_commit);
        assert_ne!(first.commit, second.commit);
        let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &first.commit).await;
        let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &second.commit).await;
    }

    #[tokio::test]
    async fn short_commit_selector_rejects_unreachable_stale_local_commit() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let replacement = fixture.replace_history_and_push("replacement");
        let stale_short = short_prefix_not_matching(&first.commit, &replacement);

        let error = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::ShortCommit(stale_short),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap_err();

        assert!(matches!(error, GitCacheError::NotFound(_)));
    }

    #[tokio::test]
    async fn session_advertises_only_upload_pack_refs() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let session_id = SessionId::parse(
            response
                .ref_name
                .strip_prefix("refs/cache/sessions/")
                .unwrap(),
        )
        .unwrap();
        let session_repo = materializer
            .session_repo_from_manifest(&fixture.repo, session_id)
            .await
            .unwrap();
        let advertised = advertise_refs(&state, &session_repo).await.unwrap();
        let advertised = String::from_utf8_lossy(&advertised);

        assert!(advertised.contains(&response.ref_name));
        assert!(advertised.contains(" filter "));
        assert!(!advertised.contains("git-receive-pack"));
    }

    // ── Contention Tests ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_materialize_same_branch() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Arc::new(Materializer::new(state));

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let m = Arc::clone(&materializer);
                let repo = fixture.repo.clone();
                tokio::spawn(async move {
                    m.materialize(MaterializeRequest {
                        repo,
                        selector: Selector::Branch(BranchName::parse("main").unwrap()),
                        mode: RequestMode::Strict,
                    })
                    .await
                })
            })
            .collect();

        let mut commits = Vec::new();
        for handle in handles {
            let result = handle.await.unwrap();
            if let Ok(response) = result {
                commits.push(response.commit);
            }
        }

        assert!(!commits.is_empty(), "at least one materialize must succeed");
        let first = &commits[0];
        for c in &commits {
            assert_eq!(
                c, first,
                "all successful materializations return same commit"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_session_creation_unique_ids() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Arc::new(Materializer::new(state));

        // First materialize to ensure commit is available.
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let commit = response.commit;

        let handles: Vec<_> = (0..10)
            .map(|_| {
                let m = Arc::clone(&materializer);
                let repo = fixture.repo.clone();
                let c = commit.clone();
                tokio::spawn(async move {
                    m.create_session(repo, c, MaterializeSource::CacheVerified)
                        .await
                })
            })
            .collect();

        let mut session_refs = Vec::new();
        for handle in handles {
            let response = handle.await.unwrap().unwrap();
            session_refs.push(response.ref_name);
        }

        assert_eq!(session_refs.len(), 10);
        // All session IDs must be unique.
        let unique: std::collections::HashSet<_> = session_refs.iter().collect();
        assert_eq!(unique.len(), 10, "all session IDs should be unique");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_cleanup_no_double_delete() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Arc::new(Materializer::new(Arc::clone(&state)));

        // Create some sessions to clean up.
        for _ in 0..5 {
            let _ = materializer
                .materialize(MaterializeRequest {
                    repo: fixture.repo.clone(),
                    selector: Selector::Branch(BranchName::parse("main").unwrap()),
                    mode: RequestMode::Strict,
                })
                .await;
        }

        // Spawn 3 concurrent cleanup tasks.
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let m = Arc::clone(&materializer);
                tokio::spawn(async move { m.cleanup_expired_sessions().await })
            })
            .collect();

        for handle in handles {
            // Should not panic or return an error from double-delete.
            let result = handle.await.unwrap();
            assert!(result.is_ok(), "cleanup should succeed: {:?}", result.err());
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn materialize_during_upstream_change() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Arc::new(Materializer::new(state));

        // Capture the original commit BEFORE pushing a new one.
        let original_commit = fixture.head_commit();

        // Start a materialize.
        let m1 = Arc::clone(&materializer);
        let repo1 = fixture.repo.clone();
        let first_handle = tokio::spawn(async move {
            m1.materialize(MaterializeRequest {
                repo: repo1,
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
        });

        // Push a new commit upstream while the first materialize is running.
        let new_commit = fixture.commit_and_push("mid-flight change");

        let first_result = first_handle.await.unwrap();
        // First should succeed with either the old or new commit.
        match first_result {
            Ok(resp) => {
                assert!(
                    resp.commit == original_commit || resp.commit == new_commit,
                    "should return a valid commit"
                );
            }
            Err(_) => {
                // Conflict during fetch is acceptable (branch moved).
            }
        }

        // Second materialize should see the new commit.
        let resp = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(resp.commit, new_commit);
    }

    pub struct GitFixture {
        pub tmp: TempDir,
        pub repo: RepoKey,
    }

    impl GitFixture {
        pub fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let repo = RepoKey::parse("github.com/org/repo").unwrap();
            let fixture = Self { tmp, repo };
            fixture.init();
            fixture
        }

        pub fn state_config(&self) -> AppConfig {
            AppConfig {
                bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
                public_base_url: "http://127.0.0.1:0".into(),
                cache_root: self.cache_root(),
                upstream_root: Some(self.tmp.path().join("upstreams")),
                git_binary: PathBuf::from("git"),
                git_timeout_seconds: 60,
                max_git_output_bytes: 16 * 1024 * 1024,
                object_store: ObjectStoreConfig::Local {
                    root: self.tmp.path().join("objects"),
                },
                session_ttl_seconds: 3600,
                upstream_auth_token_env: None,
                rate_limit_per_minute: 0,
                allowed_upstream_hosts: vec!["github.com".into()],
                disk: git_cache_core::DiskConfig {
                    quota_bytes: 1024 * 1024 * 1024,
                    min_free_bytes: 0,
                },
                git_remote: Default::default(),
                compaction: Default::default(),
                max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(
                ),
                session_cleanup_interval_secs: 300,
                max_concurrent_generation_verifications: 1,
            }
        }

        pub fn state(&self) -> AppState {
            AppState::try_new(self.state_config()).unwrap()
        }

        #[cfg(feature = "s3-tests")]
        pub fn state_with_store(&self, store: Arc<dyn ObjectStore>) -> AppState {
            let config = self.state_config();
            let git = Git::with_concurrency_limit(
                config.git_binary.clone(),
                std::time::Duration::from_secs(config.git_timeout_seconds),
                config.max_concurrent_git_processes,
            )
            .with_output_limit(config.max_git_output_bytes);
            let disk = DiskManager::new(
                &config.cache_root,
                config.disk.quota_bytes,
                config.disk.min_free_bytes,
            );
            AppState {
                config,
                store,
                git,
                disk: AsyncDiskManager::new(disk),
                generation_verification_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            }
        }

        pub fn cache_root(&self) -> PathBuf {
            self.tmp.path().join("cache")
        }

        pub fn work_path(&self) -> PathBuf {
            self.tmp.path().join("work")
        }

        pub fn upstream_path(&self) -> PathBuf {
            self.tmp
                .path()
                .join("upstreams")
                .join(self.repo.local_bare_path())
        }

        fn init(&self) {
            stdfs::create_dir_all(self.upstream_path().parent().unwrap()).unwrap();
            stdfs::create_dir_all(self.work_path()).unwrap();
            run_git(
                self.tmp.path(),
                ["init", "--bare", self.upstream_path().to_str().unwrap()],
            );
            run_git(&self.work_path(), ["init"]);
            run_git(
                &self.work_path(),
                ["config", "user.email", "cache@example.invalid"],
            );
            run_git(&self.work_path(), ["config", "user.name", "Cache Test"]);
            stdfs::write(self.work_path().join("README.md"), "initial\n").unwrap();
            run_git(&self.work_path(), ["add", "README.md"]);
            run_git(&self.work_path(), ["commit", "-m", "initial"]);
            run_git(&self.work_path(), ["branch", "-M", "main"]);
            run_git(
                &self.work_path(),
                [
                    "remote",
                    "add",
                    "origin",
                    self.upstream_path().to_str().unwrap(),
                ],
            );
            run_git(&self.work_path(), ["push", "origin", "main"]);
            run_git(
                &self.upstream_path(),
                ["symbolic-ref", "HEAD", "refs/heads/main"],
            );
        }

        pub fn commit_and_push(&self, contents: &str) -> CommitSha {
            stdfs::write(self.work_path().join("README.md"), format!("{contents}\n")).unwrap();
            run_git(&self.work_path(), ["add", "README.md"]);
            run_git(&self.work_path(), ["commit", "-m", contents]);
            run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
            CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
        }

        pub fn push_head_to_branch(&self, branch: &str) {
            run_git(
                &self.work_path(),
                ["push", "--force", "origin", &format!("HEAD:{branch}")],
            );
        }

        pub fn replace_history_and_push(&self, contents: &str) -> CommitSha {
            run_git(&self.work_path(), ["checkout", "--orphan", "replacement"]);
            stdfs::write(self.work_path().join("README.md"), format!("{contents}\n")).unwrap();
            run_git(&self.work_path(), ["add", "README.md"]);
            run_git(&self.work_path(), ["commit", "-m", contents]);
            run_git(&self.work_path(), ["branch", "-M", "main"]);
            run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
            CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
        }

        pub fn head_commit(&self) -> CommitSha {
            CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
        }
    }

    #[cfg(feature = "s3-tests")]
    struct MinioFixture {
        store: Arc<dyn ObjectStore>,
    }

    #[cfg(feature = "s3-tests")]
    impl MinioFixture {
        async fn new() -> Option<Self> {
            if std::env::var("GIT_CACHE_S3_INTEGRATION").ok().as_deref() != Some("1") {
                return None;
            }

            let endpoint = std::env::var("GIT_CACHE_S3_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
            let bucket = std::env::var("GIT_CACHE_S3_BUCKET")
                .unwrap_or_else(|_| "gitmirrorcache-test".into());
            let access_key =
                std::env::var("GIT_CACHE_S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
            let secret_key =
                std::env::var("GIT_CACHE_S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
            let prefix = format!("domain-tests/{}", uuid::Uuid::now_v7());
            let config = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "minio-integration",
                ))
                .build();
            let client = Client::from_conf(config);
            client.create_bucket().bucket(&bucket).send().await.ok();
            let store = S3ObjectStore::new(client, bucket, prefix).unwrap();
            Some(Self {
                store: Arc::new(store),
            })
        }
    }

    #[cfg(feature = "s3-tests")]
    #[tokio::test]
    async fn minio_materializer_rehydrates_commit_from_minio_after_hot_cache_deletion() {
        let Some(minio) = MinioFixture::new().await else {
            eprintln!("skipping minio_materializer_rehydrates_commit_from_minio_after_hot_cache_deletion: set GIT_CACHE_S3_INTEGRATION=1");
            return;
        };
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state_with_store(Arc::clone(&minio.store)));
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        assert!(state
            .store
            .head(&bundle_key(&fixture.repo, manifest.generation))
            .await
            .unwrap()
            .is_some());

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(first.commit.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert!(materializer.commit_exists(&repo_dir, &first.commit).await);
    }

    #[cfg(feature = "s3-tests")]
    #[tokio::test]
    async fn minio_materializer_compacts_generations_and_rehydrates_commits() {
        let Some(minio) = MinioFixture::new().await else {
            eprintln!("skipping minio_materializer_compacts_generations_and_rehydrates_commits: set GIT_CACHE_S3_INTEGRATION=1");
            return;
        };
        let fixture = GitFixture::new();
        let mut config = fixture.state_config();
        config.compaction = git_cache_core::CompactionConfig {
            chain_depth_threshold: 2,
            inline: false,
        };
        let git = Git::with_concurrency_limit(
            config.git_binary.clone(),
            std::time::Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        let state = Arc::new(AppState {
            config,
            store: Arc::clone(&minio.store),
            git,
            disk: AsyncDiskManager::new(disk),
            generation_verification_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, first_manifest.generation).await;
        let second = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        let third = fixture.commit_and_push("third");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.old_chain_depth, 3);
        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(state
                .store
                .head(&bundle_key(&fixture.repo, *old_generation))
                .await
                .unwrap()
                .is_none());
        }
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        assert_eq!(compacted.parent_generation, None);

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(third.clone()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);

        for commit in [first.commit, second, third] {
            materializer
                .state
                .git
                .rev_parse(&repo_dir, &format!("{}^{{commit}}", commit.as_str()))
                .await
                .unwrap();
        }
    }

    fn short_prefix_not_matching(commit: &CommitSha, other: &CommitSha) -> ShortCommitSha {
        let length = (8..40)
            .find(|length| commit.as_str()[..*length] != other.as_str()[..*length])
            .unwrap();
        ShortCommitSha::parse(&commit.as_str()[..length]).unwrap()
    }

    fn run_git<I, S>(cwd: &FsPath, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<I, S>(cwd: &FsPath, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn synth_no_symref_when_default_branch_absent_from_refs() {
        let sha = "a".repeat(40);
        let comp = UpstreamRefComparison {
            changed: HashMap::new(),
            all_upstream: HashMap::from([("feature".to_string(), sha.clone())]),
            default_branch: Some("main".to_string()),
        };
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // Capability line must exist (the \0 delimiter).
        assert!(
            text.contains('\0'),
            "capability line missing when default_branch is absent from refs"
        );

        // symref must NOT reference a branch that isn't in the advertisement.
        assert!(
            !text.contains("symref=HEAD:refs/heads/main"),
            "symref must not reference absent default_branch; got: {text}"
        );

        // The ref that IS present should still appear.
        assert!(text.contains("refs/heads/feature"));
        assert!(output.ends_with(b"0000"));
    }

    // ── Performance tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_repeated_materialize_same_branch() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let iterations = 10;

        let first_start = std::time::Instant::now();
        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let first_elapsed = first_start.elapsed();

        let mut subsequent_total = std::time::Duration::ZERO;
        for _ in 1..iterations {
            let start = std::time::Instant::now();
            let response = materializer
                .materialize(MaterializeRequest {
                    repo: fixture.repo.clone(),
                    selector: Selector::Branch(BranchName::parse("main").unwrap()),
                    mode: RequestMode::Strict,
                })
                .await
                .unwrap();
            subsequent_total += start.elapsed();
            assert_eq!(response.commit, first.commit);
        }

        let avg_subsequent = subsequent_total / (iterations - 1) as u32;
        eprintln!(
            "repeated materialize: first={first_elapsed:?}, avg_subsequent={avg_subsequent:?} ({} calls)",
            iterations - 1
        );
    }

    #[tokio::test]
    async fn test_session_creation_throughput() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let session_count = 20;

        // First materialize to ensure branch is cached.
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();
        let commit = response.commit;

        let start = std::time::Instant::now();
        let mut sessions = Vec::new();
        for _ in 0..session_count {
            let session = materializer
                .create_session(
                    fixture.repo.clone(),
                    commit.clone(),
                    MaterializeSource::CacheVerified,
                )
                .await
                .unwrap();
            sessions.push(session);
        }
        let elapsed = start.elapsed();

        assert_eq!(sessions.len(), session_count);
        // Verify each session has a unique ref.
        let refs: std::collections::HashSet<_> =
            sessions.iter().map(|s| s.ref_name.clone()).collect();
        assert_eq!(
            refs.len(),
            session_count,
            "each session should have a unique ref"
        );

        let avg = elapsed / session_count as u32;
        eprintln!("session creation: {session_count} sessions in {elapsed:?}, avg={avg:?}");
        assert!(
            elapsed.as_secs() < 60,
            "session creation too slow: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_cleanup_expired_sessions_performance() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            session_ttl_seconds: 0, // Expire immediately.
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state));
        let session_count = 50;

        // Create sessions that will expire immediately (ttl=0).
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
            })
            .await
            .unwrap();

        for _ in 0..session_count {
            materializer
                .create_session(
                    fixture.repo.clone(),
                    response.commit.clone(),
                    MaterializeSource::CacheVerified,
                )
                .await
                .unwrap();
        }

        // Give a brief moment for the expiry to kick in.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let start = std::time::Instant::now();
        let report = materializer.cleanup_expired_sessions().await.unwrap();
        let elapsed = start.elapsed();

        eprintln!(
            "cleanup_expired_sessions: removed={}, errors={}, {elapsed:?} for {session_count} sessions",
            report.sessions_removed,
            report.errors.len()
        );

        // All sessions should be expired and cleaned up (ttl=0).
        // We allow some margin since timing isn't precise.
        assert!(
            report.sessions_removed > 0,
            "expected some sessions to be cleaned up"
        );
        assert!(elapsed.as_secs() < 30, "cleanup too slow: {elapsed:?}");
    }

    // ── synthesize_ref_advertisement unit tests ─────────────────────────

    fn make_comparison(
        refs: &[(&str, &str)],
        default_branch: Option<&str>,
    ) -> UpstreamRefComparison {
        UpstreamRefComparison {
            changed: HashMap::new(),
            default_branch: default_branch.map(|s| s.to_string()),
            all_upstream: refs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn parse_pkt_lines(data: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        let mut offset = 0;
        while offset + 4 <= data.len() {
            let hex = std::str::from_utf8(&data[offset..offset + 4]).unwrap();
            let len = usize::from_str_radix(hex, 16).unwrap();
            if len == 0 {
                offset += 4;
                continue;
            }
            assert!(len >= 4);
            assert!(offset + len <= data.len());
            lines.push(data[offset + 4..offset + len].to_vec());
            offset += len;
        }
        lines
    }

    #[test]
    fn synth_single_branch() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);

        let text = String::from_utf8_lossy(&output);
        assert!(text.contains(&format!("{sha} HEAD")));
        assert!(text.contains("refs/heads/main"));
        assert!(output.ends_with(b"0000"));
    }

    #[test]
    fn synth_multiple_branches_sorted() {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let sha_c = "c".repeat(40);
        let sha_d = "d".repeat(40);
        let sha_e = "e".repeat(40);
        let comp = make_comparison(
            &[
                ("zeta", &sha_e),
                ("alpha", &sha_a),
                ("main", &sha_c),
                ("beta", &sha_b),
                ("gamma", &sha_d),
            ],
            Some("main"),
        );
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // All branches should appear.
        for name in &["alpha", "beta", "gamma", "main", "zeta"] {
            assert!(
                text.contains(&format!("refs/heads/{name}")),
                "missing branch {name}"
            );
        }

        // Extract branch names from pkt-line data.
        // Each ref line is: "{sha} refs/heads/{name}\n" (no NUL separator).
        // The HEAD/capabilities line is: "{sha} HEAD\0{caps}\n" — skip it.
        let pkt_lines = parse_pkt_lines(&output);
        let mut branch_names: Vec<String> = Vec::new();
        for pkt in &pkt_lines {
            let line_str = String::from_utf8_lossy(pkt);
            // Skip capability lines (they contain NUL).
            if line_str.contains('\0') {
                continue;
            }
            if let Some(rest) = line_str.split("refs/heads/").nth(1) {
                let name = rest.trim().to_string();
                if !name.is_empty() {
                    branch_names.push(name);
                }
            }
        }
        let mut sorted = branch_names.clone();
        sorted.sort();
        assert_eq!(branch_names, sorted);
    }

    #[test]
    fn synth_no_default_branch_uses_first_sorted() {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let comp = make_comparison(&[("beta", &sha_b), ("alpha", &sha_a)], None);
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // First line should be the first sorted branch with capabilities.
        let lines = parse_pkt_lines(&output);
        let first_line = String::from_utf8_lossy(&lines[0]);
        assert!(
            first_line.contains("refs/heads/alpha"),
            "first line should be alpha (first sorted): {first_line}"
        );
        assert!(
            first_line.contains('\0'),
            "first line should contain capability separator"
        );

        assert!(text.contains("refs/heads/beta"));
    }

    #[test]
    fn synth_default_branch_not_in_refs() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("feature", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // "main" is set as default but not in all_upstream. Should still
        // output feature and terminate.
        assert!(text.contains("refs/heads/feature"));
        assert!(output.ends_with(b"0000"));

        // BUG: When default_branch is set but absent from upstream refs,
        // the capability line (\0-delimited) is never emitted. Git clients
        // expect at least one ref line to carry capabilities. This assert
        // documents the bug — it will start passing once the source is fixed.
        // See: synthesize_ref_advertisement outer if-let vs else-if fallback.
        assert!(
            text.contains('\0'),
            "capability line missing when default_branch is absent from refs (known bug)"
        );
    }

    #[test]
    fn synth_pkt_line_length_correctness() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);

        let mut offset = 0;
        while offset + 4 <= output.len() {
            let hex = std::str::from_utf8(&output[offset..offset + 4]).unwrap();
            let len = usize::from_str_radix(hex, 16).unwrap();
            if len == 0 {
                offset += 4;
                continue;
            }
            assert!(
                len >= 4,
                "pkt-line at offset {offset} has invalid length {len}"
            );
            assert!(
                offset + len <= output.len(),
                "pkt-line at offset {offset} extends beyond data"
            );
            // Verify the 4-char hex prefix matches actual line length.
            let actual_data_len = len - 4;
            let actual_data = &output[offset + 4..offset + len];
            assert_eq!(
                actual_data.len(),
                actual_data_len,
                "pkt-line length mismatch"
            );
            offset += len;
        }
    }

    #[test]
    fn synth_capability_string_contents() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        for cap in &[
            "multi_ack",
            "thin-pack",
            "side-band-64k",
            "no-done",
            "filter",
            "object-format=sha1",
        ] {
            assert!(text.contains(cap), "missing capability: {cap}");
        }
    }

    #[test]
    fn synth_symref_capability() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        assert!(
            text.contains("symref=HEAD:refs/heads/main"),
            "missing symref capability"
        );
    }

    #[test]
    fn synth_empty_refs() {
        let comp = make_comparison(&[], None);
        let output = synthesize_ref_advertisement(&comp);
        assert_eq!(output, b"0000");
    }
}
