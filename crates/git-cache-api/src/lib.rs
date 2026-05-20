use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use chrono::{Duration as ChronoDuration, Utc};
use git_cache_core::{
    AppConfig, BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest,
    GitCacheError, MaterializeRequest, MaterializeResponse, MaterializeSource, ObjectStoreConfig,
    RefManifest, RepoKey, RequestMode, Result as CoreResult, Selector, SessionId, SessionManifest,
    ShortCommitSha,
};
use git_cache_disk::DiskManager;
use git_cache_git::Git;
use git_cache_objectstore::{
    read_commit_manifest, read_generation_manifest, read_json, read_ref_manifest,
    read_session_manifest, write_json, write_ref_manifest, write_session_manifest,
    GenerationPublish, PublishManifests,
};
use git_cache_objectstore::{LocalObjectStore, ObjectStore};
use http::{header, Method, StatusCode, Uri};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::fs;
use tracing::debug;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    store: Arc<dyn ObjectStore>,
    git: Git,
    disk: DiskManager,
    metrics: Arc<Metrics>,
    rate_limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn try_new(config: AppConfig) -> CoreResult<Self> {
        let store: Arc<dyn ObjectStore> = match &config.object_store {
            ObjectStoreConfig::Local { root } => Arc::new(LocalObjectStore::new(root)),
            ObjectStoreConfig::S3 { .. } => {
                return Err(GitCacheError::NotImplemented(
                    "S3 object store wiring is provided by the objectstore crate and not enabled in the API yet"
                        .into(),
                ))
            }
        };

        let git = Git::new(
            config.git_binary.clone(),
            Duration::from_secs(config.git_timeout_seconds),
        )
        .with_output_limit(config.max_git_output_bytes);
        let git = with_optional_upstream_credentials(git, &config);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        let rate_limiter = RateLimiter::new(config.rate_limit_per_minute);

        Ok(Self {
            config,
            store,
            git,
            disk,
            metrics: Arc::new(Metrics::default()),
            rate_limiter: Arc::new(rate_limiter),
        })
    }
}

fn with_optional_upstream_credentials(git: Git, config: &AppConfig) -> Git {
    let Some(token_env) = &config.upstream_auth_token_env else {
        return git;
    };
    let Ok(token) = std::env::var(token_env) else {
        return git;
    };
    if token.trim().is_empty() {
        return git;
    }

    let host = config
        .allowed_upstream_hosts
        .first()
        .map(String::as_str)
        .unwrap_or("github.com");

    git.with_env("GIT_CONFIG_COUNT", "1")
        .with_env(
            "GIT_CONFIG_KEY_0",
            format!("http.https://{host}/.extraHeader"),
        )
        .with_env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
}

pub fn app(config: AppConfig) -> Router {
    app_result(config).expect("failed to initialize git-cache-api")
}

pub fn app_result(config: AppConfig) -> CoreResult<Router> {
    let state = Arc::new(AppState::try_new(config)?);

    Ok(Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/v1/materialize", post(materialize))
        .route("/v1/resolve", post(resolve))
        .route("/git/session/{session_id}/{*repo_path}", any(git_session))
        .with_state(state))
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        checked_at: Utc::now(),
    })
}

async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let body = format!(
        "git_cache_materialize_total {}\n\
         git_cache_materialize_errors_total {}\n\
         git_cache_git_upload_pack_total {}\n\
         git_cache_rate_limited_total {}\n",
        state.metrics.materialize_total.load(Ordering::Relaxed),
        state
            .metrics
            .materialize_errors_total
            .load(Ordering::Relaxed),
        state.metrics.upload_pack_total.load(Ordering::Relaxed),
        state.metrics.rate_limited_total.load(Ordering::Relaxed),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(body))
        .expect("metrics response")
}

async fn materialize(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    if !state.rate_limiter.check() {
        state
            .metrics
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limit exceeded".into(),
        });
    }

    state
        .metrics
        .materialize_total
        .fetch_add(1, Ordering::Relaxed);
    let materializer = Materializer::new(Arc::clone(&state));
    match materializer.materialize(request).await {
        Ok(response) => Ok(Json(response).into_response()),
        Err(error) => {
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            Err(error.into())
        }
    }
}

async fn resolve(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let materializer = Materializer::new(state);
    let response = materializer.materialize(request).await?;
    Ok(Json(response).into_response())
}

async fn git_session(
    State(state): State<Arc<AppState>>,
    Path((session_id, repo_path)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let path = uri.path();
    if path.contains("git-receive-pack")
        || query
            .get("service")
            .is_some_and(|service| service == "git-receive-pack")
    {
        return ApiError::from(GitCacheError::Unsupported(
            "git-receive-pack is disabled".into(),
        ))
        .into_response();
    }

    let session_id = match SessionId::parse(&session_id) {
        Ok(id) => id,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let repo = match repo_from_git_path(&repo_path) {
        Ok(repo) => repo,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let materializer = Materializer::new(Arc::clone(&state));
    let session_repo = match materializer
        .session_repo_from_manifest(&repo, session_id)
        .await
    {
        Ok(repo) => repo,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let result = if method == Method::GET
        && path.ends_with("/info/refs")
        && query
            .get("service")
            .is_some_and(|service| service == "git-upload-pack")
    {
        state
            .metrics
            .upload_pack_total
            .fetch_add(1, Ordering::Relaxed);
        advertise_refs(&state, &session_repo).await.map(|output| {
            let mut framed = Vec::with_capacity(output.len() + 34);
            framed.extend_from_slice(b"001e# service=git-upload-pack\n0000");
            framed.extend_from_slice(&output);
            git_response("application/x-git-upload-pack-advertisement", framed)
        })
    } else if method == Method::POST && path.ends_with("/git-upload-pack") {
        state
            .metrics
            .upload_pack_total
            .fetch_add(1, Ordering::Relaxed);
        upload_pack(&state, &session_repo, body)
            .await
            .map(|output| git_response("application/x-git-upload-pack-result", output))
    } else {
        Err(GitCacheError::Unsupported(format!(
            "unsupported git session request: {method} {path}"
        )))
    };

    match result {
        Ok(response) => response,
        Err(error) => ApiError::from(error).into_response(),
    }
}

fn git_response(content_type: &'static str, output: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(output))
        .expect("git response")
}

struct Materializer {
    state: Arc<AppState>,
}

impl Materializer {
    fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    async fn materialize(&self, request: MaterializeRequest) -> CoreResult<MaterializeResponse> {
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

    async fn materialize_short_commit(
        &self,
        repo: RepoKey,
        short_commit: ShortCommitSha,
    ) -> CoreResult<MaterializeResponse> {
        let repo_dir = self.ensure_repo_dir(&repo).await?;
        if let Ok(commit) = self.resolve_short_commit(&repo_dir, &short_commit).await {
            if let Some(manifest) = self.get_commit_manifest(&repo, &commit).await? {
                if manifest.complete {
                    self.hydrate_commit(&manifest).await?;
                    return self
                        .create_session(repo, commit, MaterializeSource::CacheVerified)
                        .await;
                }
            }
        }

        self.fetch_all_refs(&repo, &repo_dir).await?;
        let commit = self.resolve_short_commit(&repo_dir, &short_commit).await?;
        self.materialize_commit(repo, commit).await
    }

    async fn materialize_commit(
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

        let generation = self
            .publish_generation(&repo, &repo_dir, &commit, None)
            .await?;
        debug!(%repo, %commit, %generation, "published generation for exact commit");
        self.create_session(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn materialize_branch(
        &self,
        repo: RepoKey,
        branch: BranchName,
        mode: RequestMode,
        default_branch: bool,
    ) -> CoreResult<MaterializeResponse> {
        if mode == RequestMode::Cached {
            if let Some(ref_manifest) = self.get_branch_manifest(&repo, &branch).await? {
                if self.ref_is_fresh(&ref_manifest) {
                    self.hydrate_ref(&ref_manifest).await?;
                    return self
                        .create_session(repo, ref_manifest.commit, MaterializeSource::CacheVerified)
                        .await;
                }
            }

            return Err(GitCacheError::UpstreamUnavailable(format!(
                "cached branch `{branch}` is unavailable or stale"
            )));
        }

        let upstream_commit = self.ls_remote_branch(&repo, &branch).await?;
        let repo_dir = self.ensure_repo_dir(&repo).await?;
        let local_ref = format!("refs/cache/upstream/heads/{}", branch.as_str());
        self.state
            .git
            .fetch_branch(
                &repo_dir,
                &self.upstream_url(&repo)?,
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
        self.publish_generation(&repo, &repo_dir, &commit, Some(branch.clone()))
            .await?;

        if default_branch {
            self.put_default_manifest(&repo, &commit).await?;
        }

        self.create_session(repo, commit, MaterializeSource::GithubVerified)
            .await
    }

    async fn materialize_default_branch(
        &self,
        repo: RepoKey,
        mode: RequestMode,
    ) -> CoreResult<MaterializeResponse> {
        if mode == RequestMode::Cached {
            if let Some(ref_manifest) = self.get_default_manifest(&repo).await? {
                if self.ref_is_fresh(&ref_manifest) {
                    self.hydrate_ref(&ref_manifest).await?;
                    return self
                        .create_session(repo, ref_manifest.commit, MaterializeSource::CacheVerified)
                        .await;
                }
            }

            return Err(GitCacheError::UpstreamUnavailable(
                "cached default branch is unavailable or stale".into(),
            ));
        }

        let branch = self.resolve_default_branch(&repo).await?;
        self.materialize_branch(repo, branch, RequestMode::Strict, true)
            .await
    }

    async fn create_session(
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

    async fn session_repo_from_manifest(
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

            let reservation = self.state.disk.reserve(bundle.len() as u64)?;
            let temp_path = reservation.temp_path();
            fs::create_dir_all(&temp_path).await?;
            let bundle_path = temp_path.join("hydrate.bundle");
            fs::write(&bundle_path, bundle).await?;
            self.state.git.fetch_bundle(repo_dir, &bundle_path).await?;
            self.state.git.fsck(repo_dir).await?;
        }
        Ok(())
    }

    async fn publish_generation(
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
        let generation = GenerationId(
            Utc::now()
                .timestamp_nanos_opt()
                .unwrap_or_else(|| Utc::now().timestamp_millis() * 1_000_000) as u64,
        );
        let bundle_key = bundle_key(repo, generation);

        let reservation = self.state.disk.reserve(1024 * 1024 * 64)?;
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

    async fn get_commit_manifest(
        &self,
        repo: &RepoKey,
        commit: &CommitSha,
    ) -> CoreResult<Option<CommitManifest>> {
        read_commit_manifest(&*self.state.store, repo, commit).await
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

    async fn ensure_repo_dir(&self, repo: &RepoKey) -> CoreResult<PathBuf> {
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

    async fn fetch_all_refs(&self, repo: &RepoKey, repo_dir: &FsPath) -> CoreResult<()> {
        let remote = self.upstream_url(repo)?;
        self.state
            .git
            .run(
                Some(repo_dir),
                [
                    "fetch",
                    "--no-tags",
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

    fn repo_dir(&self, repo: &RepoKey) -> PathBuf {
        self.state
            .config
            .cache_root
            .join("repos")
            .join(repo.local_bare_path())
    }

    fn session_repo_path(&self, session_id: SessionId) -> PathBuf {
        self.state
            .config
            .cache_root
            .join("sessions")
            .join(format!("{session_id}.git"))
    }

    fn ref_is_fresh(&self, manifest: &RefManifest) -> bool {
        let max_age =
            ChronoDuration::seconds(self.state.config.cached_ref_max_staleness_seconds as i64);
        Utc::now() - manifest.verified_at <= max_age
    }

    fn validate_host(&self, repo: &RepoKey) -> CoreResult<()> {
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

    fn upstream_url(&self, repo: &RepoKey) -> CoreResult<String> {
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
}

async fn advertise_refs(state: &AppState, repo: &FsPath) -> CoreResult<Vec<u8>> {
    Ok(state
        .git
        .upload_pack_advertise_refs(repo, state.config.max_git_output_bytes)
        .await?
        .stdout)
}

async fn upload_pack(state: &AppState, repo: &FsPath, body: Bytes) -> CoreResult<Vec<u8>> {
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

#[cfg(test)]
fn ref_manifest_key(repo: &RepoKey, branch: &str) -> String {
    git_cache_objectstore::ref_manifest_key(repo, &format!("refs/heads/{branch}"))
        .expect("validated branch ref")
}

fn default_manifest_key(repo: &RepoKey) -> String {
    format!("repos/{}/manifests/refs/default.json", repo.as_str())
}

fn bundle_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!(
        "repos/{}/generations/{}/base.bundle",
        repo.as_str(),
        generation
    )
}

fn repo_from_git_path(repo_path: &str) -> CoreResult<RepoKey> {
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

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    checked_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Default)]
struct Metrics {
    materialize_total: AtomicU64,
    materialize_errors_total: AtomicU64,
    upload_pack_total: AtomicU64,
    rate_limited_total: AtomicU64,
}

#[derive(Debug)]
struct RateLimiter {
    limit: u32,
    state: Mutex<RateLimitWindow>,
}

#[derive(Debug)]
struct RateLimitWindow {
    started: Instant,
    count: u32,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        Self {
            limit,
            state: Mutex::new(RateLimitWindow {
                started: Instant::now(),
                count: 0,
            }),
        }
    }

    fn check(&self) -> bool {
        if self.limit == 0 {
            return true;
        }

        let mut state = self.state.lock().expect("rate limiter mutex poisoned");
        if state.started.elapsed() >= Duration::from_secs(60) {
            state.started = Instant::now();
            state.count = 0;
        }

        if state.count >= self.limit {
            return false;
        }

        state.count += 1;
        true
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<GitCacheError> for ApiError {
    fn from(error: GitCacheError) -> Self {
        let status = match error {
            GitCacheError::NotFound(_) => StatusCode::NOT_FOUND,
            GitCacheError::UpstreamUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            GitCacheError::DiskFull(_) => StatusCode::INSUFFICIENT_STORAGE,
            GitCacheError::Unsupported(_) => StatusCode::METHOD_NOT_ALLOWED,
            GitCacheError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            GitCacheError::Validation(_) => StatusCode::BAD_REQUEST,
            GitCacheError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            GitCacheError::Conflict(_) => StatusCode::CONFLICT,
            GitCacheError::Io(_) | GitCacheError::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody<'a> {
            error: &'a str,
        }

        (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response()
    }
}

pub fn empty_body() -> Body {
    Body::empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;
    use std::net::SocketAddr;
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn rate_limiter_blocks_after_limit() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check());
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
    async fn short_commit_selector_uses_cached_full_commit_after_resolution() {
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

        assert_eq!(short_response.source, MaterializeSource::CacheVerified);
        assert_eq!(short_response.commit, branch_response.commit);
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
    async fn strict_branch_and_default_require_upstream() {
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

    #[tokio::test]
    async fn receive_pack_requests_are_rejected_before_session_lookup() {
        let fixture = GitFixture::new();
        let mut query = HashMap::new();
        query.insert("service".to_string(), "git-receive-pack".to_string());

        let response = git_session(
            State(Arc::new(fixture.state())),
            Path((
                "not-a-session".to_string(),
                "github.com/org/repo.git".to_string(),
            )),
            Query(query),
            Method::GET,
            Uri::from_static(
                "/git/session/not-a-session/github.com/org/repo.git/info/refs?service=git-receive-pack",
            ),
            Bytes::new(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    struct GitFixture {
        tmp: TempDir,
        repo: RepoKey,
    }

    impl GitFixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let repo = RepoKey::parse("github.com/org/repo").unwrap();
            let fixture = Self { tmp, repo };
            fixture.init();
            fixture
        }

        fn state(&self) -> AppState {
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

        fn cache_root(&self) -> PathBuf {
            self.tmp.path().join("cache")
        }

        fn work_path(&self) -> PathBuf {
            self.tmp.path().join("work")
        }

        fn upstream_path(&self) -> PathBuf {
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

        fn commit_and_push(&self, contents: &str) -> CommitSha {
            stdfs::write(self.work_path().join("README.md"), format!("{contents}\n")).unwrap();
            run_git(&self.work_path(), ["add", "README.md"]);
            run_git(&self.work_path(), ["commit", "-m", contents]);
            run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
            CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
        }

        fn head_commit(&self) -> CommitSha {
            CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
        }
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
