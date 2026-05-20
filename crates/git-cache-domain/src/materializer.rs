use crate::state::AppState;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use git_cache_core::{
    BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest, GitCacheError,
    MaterializeRequest, MaterializeResponse, MaterializeSource, RefManifest, RepoKey, RequestMode,
    Result as CoreResult, Selector, SessionId, SessionManifest, ShortCommitSha,
};
use git_cache_objectstore::{
    read_commit_manifest, read_generation_manifest, read_json, read_ref_manifest,
    read_session_manifest, write_json, write_ref_manifest, write_session_manifest,
    GenerationPublish, PublishManifests,
};
use git_cache_worker::{UpdateExecutor, UpdateRequest, UpdateTarget};
use serde::Serialize;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tracing::debug;

pub struct Materializer {
    state: Arc<AppState>,
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
            Selector::Branch(branch) => {
                self.materialize_branch_from_manifest(request.repo, &branch)
                    .await
            }
            Selector::DefaultBranch => {
                self.materialize_default_branch_from_manifest(request.repo)
                    .await
            }
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
        self.materialize_verified_commit(repo, commit, MaterializeSource::GithubVerified)
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
        self.fetch_all_refs(&repo, &repo_dir).await?;

        if !self.commit_exists(&repo_dir, &commit).await {
            return Err(GitCacheError::NotFound(format!(
                "commit `{commit}` was not found after upstream verification"
            )));
        }

        self.materialize_verified_commit(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn materialize_verified_commit(
        &self,
        repo: RepoKey,
        commit: CommitSha,
        source: MaterializeSource,
    ) -> CoreResult<MaterializeResponse> {
        if let Some(manifest) = self.get_commit_manifest(&repo, &commit).await? {
            if manifest.complete {
                self.hydrate_commit(&manifest).await?;
                return self.create_session(repo, commit, source).await;
            }
        }

        let repo_dir = self.ensure_repo_dir(&repo).await?;
        if !self.commit_exists(&repo_dir, &commit).await {
            return Err(GitCacheError::NotFound(format!(
                "verified commit `{commit}` is missing from the local repo"
            )));
        }

        let generation = self
            .publish_generation(&repo, &repo_dir, &commit, None)
            .await?;
        debug!(%repo, %commit, %generation, "published generation for exact commit");
        self.create_session(repo, commit, source).await
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

        self.state.git.fsck(&repo_dir).await?;
        self.publish_generation(repo, &repo_dir, &commit, Some(branch.clone()))
            .await?;

        if default_branch {
            self.put_default_manifest(repo, &commit).await?;
        }

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
        if !self.commit_exists(&repo_dir, &commit).await {
            return Err(GitCacheError::NotFound(format!(
                "cannot create session for missing local commit `{commit}`"
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
        if !self.commit_exists(&repo_dir, &manifest.commit).await {
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
        if self.commit_exists(&repo_dir, &manifest.commit).await {
            return Ok(());
        }

        self.hydrate_generation(&manifest.repo, &repo_dir, manifest.generation)
            .await?;
        if !self.commit_exists(&repo_dir, &manifest.commit).await {
            return Err(GitCacheError::NotFound(format!(
                "hydrated generation `{}` did not contain commit `{}`",
                manifest.generation, manifest.commit
            )));
        }
        Ok(())
    }

    async fn hydrate_ref(&self, manifest: &RefManifest) -> CoreResult<()> {
        let commit_manifest = CommitManifest {
            repo: manifest.repo.clone(),
            commit: manifest.commit.clone(),
            generation: manifest.generation,
            complete: true,
            verified_at: manifest.verified_at,
        };
        self.hydrate_commit(&commit_manifest).await
    }

    async fn hydrate_generation(
        &self,
        repo: &RepoKey,
        repo_dir: &FsPath,
        generation: GenerationId,
    ) -> CoreResult<()> {
        let mut chain = Vec::new();
        let mut next = Some(generation);
        while let Some(current) = next {
            let manifest: GenerationManifest = self
                .get_generation_manifest(repo, current)
                .await?
                .ok_or_else(|| {
                GitCacheError::NotFound(format!("generation manifest `{current}` not found"))
            })?;
            next = manifest.parent_generation;
            chain.push(manifest);
        }

        for generation_manifest in chain.iter().rev() {
            let bundle = self
                .state
                .store
                .get(&generation_manifest.bundle_key)
                .await?
                .ok_or_else(|| {
                    GitCacheError::NotFound(format!(
                        "bundle `{}` not found",
                        generation_manifest.bundle_key
                    ))
                })?;

            let reservation = self.state.disk.reserve(bundle.len() as u64).await?;
            let temp_path = reservation.temp_path();
            fs::create_dir_all(&temp_path).await?;
            let bundle_path = temp_path.join("hydrate.bundle");
            fs::write(&bundle_path, bundle).await?;
            self.state.git.fetch_bundle(repo_dir, &bundle_path).await?;
            self.state.git.fsck(repo_dir).await?;
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

                return Ok(existing.generation);
            }
        }

        self.state.git.fsck(repo_dir).await?;
        let generation = GenerationId::new();
        let bundle_key = bundle_key(repo, generation);

        let reservation = self.state.disk.reserve(1024 * 1024 * 64).await?;
        let temp_path = reservation.temp_path();
        fs::create_dir_all(&temp_path).await?;
        let bundle_path = temp_path.join("generation.bundle");
        self.state
            .git
            .bundle_create_all(repo_dir, &bundle_path)
            .await?;
        let now = Utc::now();
        let generation_manifest = GenerationManifest {
            repo: repo.clone(),
            generation,
            bundle_key,
            parent_generation: None,
            created_at: now,
            commits: vec![commit.clone()],
        };
        let commit_manifest = CommitManifest {
            repo: repo.clone(),
            commit: commit.clone(),
            generation,
            complete: true,
            verified_at: now,
        };
        let mut manifests = PublishManifests {
            commits: vec![commit_manifest],
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

        GenerationPublish::with_manifests(generation_manifest, manifests)
            .publish_bundle_file(&*self.state.store, &bundle_path)
            .await?;

        reservation.release().await?;

        Ok(generation)
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

    async fn materialize_branch_from_manifest(
        &self,
        repo: RepoKey,
        branch: &BranchName,
    ) -> CoreResult<MaterializeResponse> {
        let manifest = self
            .get_branch_manifest(&repo, branch)
            .await?
            .ok_or_else(|| {
                GitCacheError::NotFound(format!("branch `{branch}` manifest missing"))
            })?;
        self.hydrate_ref(&manifest).await?;
        self.create_session(repo, manifest.commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn materialize_default_branch_from_manifest(
        &self,
        repo: RepoKey,
    ) -> CoreResult<MaterializeResponse> {
        let manifest = self
            .get_default_manifest(&repo)
            .await?
            .ok_or_else(|| GitCacheError::NotFound("default branch manifest missing".into()))?;
        self.hydrate_ref(&manifest).await?;
        self.create_session(repo, manifest.commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn get_branch_manifest(
        &self,
        repo: &RepoKey,
        branch: &BranchName,
    ) -> CoreResult<Option<RefManifest>> {
        read_ref_manifest(&*self.state.store, repo, &branch.ref_name()).await
    }

    async fn get_default_manifest(&self, repo: &RepoKey) -> CoreResult<Option<RefManifest>> {
        read_json(&*self.state.store, &default_manifest_key(repo)).await
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
            if let Some(parent) = repo_dir.parent() {
                fs::create_dir_all(parent).await?;
            }
            self.state.git.init_bare(&repo_dir).await?;
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
        let output = self
            .state
            .git
            .run(
                Some(repo_dir),
                [
                    "for-each-ref",
                    "--format=%(refname)",
                    "refs/cache/upstream/heads",
                ],
            )
            .await?;
        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::Validation(format!("git for-each-ref returned non-utf8: {err}"))
        })?;

        for ref_name in text.lines().filter(|line| !line.trim().is_empty()) {
            if self
                .state
                .git
                .run(
                    Some(repo_dir),
                    ["merge-base", "--is-ancestor", commit.as_str(), ref_name],
                )
                .await
                .is_ok()
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    pub async fn fetch_all_refs(&self, repo: &RepoKey, repo_dir: &FsPath) -> CoreResult<()> {
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
        self.state.git.fsck(repo_dir).await?;
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
            "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
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

    pub async fn cleanup_expired_sessions(&self) -> CoreResult<SessionCleanupReport> {
        let keys = self.state.store.list_prefix("repos/").await?;
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

pub async fn upload_pack(
    state: &AppState,
    repo: &FsPath,
    body: bytes::Bytes,
) -> CoreResult<Vec<u8>> {
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
    use git_cache_core::{AppConfig, ObjectStoreConfig};
    use std::fs as stdfs;
    use std::net::SocketAddr;
    use std::process::Command;
    use tempfile::TempDir;

    #[cfg(test)]
    fn ref_manifest_key(repo: &RepoKey, branch: &str) -> String {
        git_cache_objectstore::ref_manifest_key(repo, &format!("refs/heads/{branch}"))
            .expect("validated branch ref")
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
        assert!(materializer
            .get_commit_manifest(&fixture.repo, &head)
            .await
            .unwrap()
            .is_some());
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
        assert!(materializer
            .get_commit_manifest(&fixture.repo, &first.commit)
            .await
            .unwrap()
            .is_some());
        assert!(materializer
            .get_commit_manifest(&fixture.repo, &second.commit)
            .await
            .unwrap()
            .is_some());
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
        assert!(!advertised.contains("git-receive-pack"));
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

        pub fn state(&self) -> AppState {
            AppState::try_new(AppConfig {
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
                cached_ref_max_staleness_seconds: 300,
                upstream_auth_token_env: None,
                rate_limit_per_minute: 0,
                allowed_upstream_hosts: vec!["github.com".into()],
                disk: git_cache_core::DiskConfig {
                    quota_bytes: 1024 * 1024 * 1024,
                    min_free_bytes: 0,
                },
            })
            .unwrap()
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

    fn short_prefix_not_matching(commit: &CommitSha, other: &CommitSha) -> ShortCommitSha {
        let length = (8..40)
            .find(|length| &commit.as_str()[..*length] != &other.as_str()[..*length])
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
}
