use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::Stream;
use git_cache_core::{
    AppConfig, GitCacheError, MaterializeRequest, RepoKey, Result as CoreResult, UpstreamAuth,
};
use git_cache_domain::materializer::repo_from_git_path;
pub use git_cache_domain::AppState as DomainAppState;
use git_cache_domain::{
    frame_ref_advertisement, parse_want_lines, synthesize_ref_advertisement, AppState,
    Materializer, UpstreamRefComparison,
};
use git_cache_git::UploadPackProcess;
use git_cache_worker::{LeaseAcquire, ObjectStoreRepoLeaseManager, RepoLease, RepoLeaseManager};
use http::{header, HeaderMap, Method, StatusCode, Uri};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::Sleep;
use tokio_util::io::ReaderStream;
use tracing::{info, info_span, warn, Instrument};

const GIT_UPLOAD_PACK_STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECT_GIT_PROOF_TTL: Duration = Duration::from_secs(30);
const LEASE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(100);

pub fn app(config: AppConfig) -> Router {
    app_result(config).expect("failed to initialize git-cache-api")
}

pub fn app_result(config: AppConfig) -> CoreResult<Router> {
    let git_remote_enabled = config.git_remote.enabled;
    let state = Arc::new(ApiState::try_new(config)?);
    router(git_remote_enabled, state)
}

pub async fn app_result_async(config: AppConfig) -> CoreResult<Router> {
    let git_remote_enabled = config.git_remote.enabled;
    let state = Arc::new(ApiState::try_new_async(config).await?);
    router(git_remote_enabled, state)
}

fn router(git_remote_enabled: bool, state: Arc<ApiState>) -> CoreResult<Router> {
    let git_body_limit = state.domain.config.max_git_output_bytes;
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/v1/materialize", post(materialize))
        .route("/v1/resolve", post(resolve));

    if git_remote_enabled {
        router = router.route(
            "/git/{*repo_path}",
            any(git_repo).layer(DefaultBodyLimit::max(git_body_limit)),
        );
    }

    Ok(router.with_state(state))
}

#[derive(Clone)]
struct ApiState {
    domain: Arc<AppState>,
    direct_git_proofs: Arc<DirectGitProofCache>,
    leases: Arc<dyn RepoLeaseManager>,
    metrics: Arc<Metrics>,
    rate_limiter: Arc<RateLimiter>,
}

impl ApiState {
    fn try_new(config: AppConfig) -> CoreResult<Self> {
        let rate_limiter = RateLimiter::new(config.rate_limit_per_minute);
        let domain = Arc::new(AppState::try_new(config)?);
        Self::with_domain(rate_limiter, domain)
    }

    async fn try_new_async(config: AppConfig) -> CoreResult<Self> {
        let rate_limiter = RateLimiter::new(config.rate_limit_per_minute);
        let domain = Arc::new(AppState::try_new_async(config).await?);
        Self::with_domain(rate_limiter, domain)
    }

    fn with_domain(rate_limiter: RateLimiter, domain: Arc<AppState>) -> CoreResult<Self> {
        let leases: Arc<dyn RepoLeaseManager> = Arc::new(ObjectStoreRepoLeaseManager::new(
            Arc::clone(&domain.store),
            &domain.config.leases,
        ));
        Materializer::new(Arc::clone(&domain)).enqueue_pending_generation_scan();
        Ok(Self {
            domain,
            direct_git_proofs: Arc::new(DirectGitProofCache::new(DIRECT_GIT_PROOF_TTL)),
            leases,
            metrics: Arc::new(Metrics::default()),
            rate_limiter: Arc::new(rate_limiter),
        })
    }

    fn next_request_id(&self) -> u64 {
        self.metrics.request_ids.fetch_add(1, Ordering::Relaxed) + 1
    }
}

async fn acquire_repo_write_lease_with_wait(
    state: &Arc<ApiState>,
    repo: &RepoKey,
    max_wait: Duration,
) -> CoreResult<Box<dyn RepoLease>> {
    let started_at = Instant::now();

    loop {
        match state.leases.acquire(repo).await? {
            LeaseAcquire::Acquired(lease) => return Ok(lease),
            LeaseAcquire::Busy if started_at.elapsed() < max_wait => {
                tokio::time::sleep(
                    LEASE_BUSY_RETRY_INTERVAL.min(max_wait.saturating_sub(started_at.elapsed())),
                )
                .await;
            }
            LeaseAcquire::Busy => {
                return Err(GitCacheError::LeaseBusy(format!(
                    "timed out waiting for repo-write lease for `{repo}`"
                )));
            }
        }
    }
}

async fn acquire_repo_write_lease_for_git_client(
    state: &Arc<ApiState>,
    repo: &RepoKey,
) -> CoreResult<Box<dyn RepoLease>> {
    let max_wait = Duration::from_secs(state.domain.config.git_timeout_seconds.max(1));
    acquire_repo_write_lease_with_wait(state, repo, max_wait).await
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DirectGitProofKey {
    repo: RepoKey,
    auth: DirectGitProofAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DirectGitProofAuth {
    Anonymous,
    Authenticated(String),
}

#[derive(Debug, Clone)]
struct DirectGitProof {
    inserted_at: Instant,
    comparison: UpstreamRefComparison,
}

#[derive(Debug)]
struct DirectGitProofCache {
    ttl: Duration,
    entries: Mutex<HashMap<DirectGitProofKey, DirectGitProof>>,
}

impl DirectGitProofCache {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn insert(&self, repo: &RepoKey, auth: &UpstreamAuth, comparison: UpstreamRefComparison) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        let now = Instant::now();
        self.prune_locked(&mut entries, now);
        entries.insert(
            DirectGitProofKey::new(repo, auth),
            DirectGitProof {
                inserted_at: now,
                comparison,
            },
        );
    }

    fn get(
        &self,
        repo: &RepoKey,
        auth: &UpstreamAuth,
    ) -> Option<(UpstreamAuth, UpstreamRefComparison)> {
        let Ok(mut entries) = self.entries.lock() else {
            return None;
        };
        let now = Instant::now();
        self.prune_locked(&mut entries, now);

        if auth.is_authenticated() {
            if let Some(proof) = entries.get(&DirectGitProofKey::new(repo, auth)) {
                return Some((auth.clone(), proof.comparison.clone()));
            }
            return None;
        }

        if let Some(proof) = entries.get(&DirectGitProofKey::anonymous(repo)) {
            return Some((UpstreamAuth::Anonymous, proof.comparison.clone()));
        }

        None
    }

    fn prune_locked(&self, entries: &mut HashMap<DirectGitProofKey, DirectGitProof>, now: Instant) {
        entries.retain(|_, proof| now.duration_since(proof.inserted_at) <= self.ttl);
    }
}

impl DirectGitProofKey {
    fn anonymous(repo: &RepoKey) -> Self {
        Self {
            repo: repo.clone(),
            auth: DirectGitProofAuth::Anonymous,
        }
    }

    fn new(repo: &RepoKey, auth: &UpstreamAuth) -> Self {
        Self {
            repo: repo.clone(),
            auth: DirectGitProofAuth::from_auth(auth),
        }
    }
}

impl DirectGitProofAuth {
    fn from_auth(auth: &UpstreamAuth) -> Self {
        let Some(raw) = auth.raw_header() else {
            return Self::Anonymous;
        };
        let digest = Sha256::digest(raw.as_bytes());
        Self::Authenticated(hex_lower(&digest))
    }
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

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        checked_at: chrono::Utc::now(),
    })
}

async fn metrics(State(state): State<Arc<ApiState>>) -> Response {
    let body = format!(
        "git_cache_materialize_total {}\n\
         git_cache_materialize_errors_total {}\n\
         git_cache_rate_limited_total {}\n\
         git_cache_git_remote_refs_total {}\n\
         git_cache_git_remote_upload_pack_total {}\n",
        state.metrics.materialize_total.load(Ordering::Relaxed),
        state
            .metrics
            .materialize_errors_total
            .load(Ordering::Relaxed),
        state.metrics.rate_limited_total.load(Ordering::Relaxed),
        state.metrics.git_remote_refs_total.load(Ordering::Relaxed),
        state
            .metrics
            .git_remote_upload_pack_total
            .load(Ordering::Relaxed),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(body))
        .expect("metrics response")
}

async fn materialize(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let auth = upstream_api_auth(&headers)?;
    handle_materialize_request(&state, request, auth).await
}

async fn resolve(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let auth = upstream_api_auth(&headers)?;
    handle_resolve_request(&state, request, auth).await
}

async fn handle_materialize_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let request_id = state.next_request_id();
    let repo = request.repo.clone();
    let selector = format!("{:?}", request.selector);
    let span = info_span!(
        "api_request",
        request_id,
        endpoint = "materialize",
        repo = %repo
    );
    async move {
        handle_materialize_request_inner(state, request, auth, request_id, selector).await
    }
    .instrument(span)
    .await
}

async fn handle_materialize_request_inner(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
    request_id: u64,
    selector: String,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let repo = request.repo.clone();
    info!(
        request_id,
        repo = %repo,
        selector,
        auth = auth_label(&auth),
        "materialize request started"
    );
    if !state.rate_limiter.check() {
        state
            .metrics
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
        info!(
            request_id,
            repo = %repo,
            elapsed_ms = elapsed_ms(started),
            status = %StatusCode::TOO_MANY_REQUESTS,
            "materialize request finished"
        );
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limit exceeded".into(),
            retry_after: None,
        });
    }

    state
        .metrics
        .materialize_total
        .fetch_add(1, Ordering::Relaxed);

    let CheckedMaterializeRequest { request, auth } =
        match check_materialize_upstream_auth(request, auth).await {
            Ok(checked) => checked,
            Err(error) => {
                state
                    .metrics
                    .materialize_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                info!(
                    request_id,
                    repo = %repo,
                    elapsed_ms = elapsed_ms(started),
                    status = %error.status,
                    "materialize request finished"
                );
                return Err(error);
            }
        };

    let result = run_materialize_request(state, request, auth).await;
    match &result {
        Ok(_) => info!(
            request_id,
            elapsed_ms = elapsed_ms(started),
            status = %StatusCode::OK,
            "materialize request finished"
        ),
        Err(error) => info!(
            request_id,
            elapsed_ms = elapsed_ms(started),
            status = %error.status,
            "materialize request finished"
        ),
    }
    result
}

async fn run_materialize_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let domain_started = Instant::now();
    let repo = request.repo.clone();
    let retry_after = state.domain.config.leases.busy_retry_after_seconds;
    let lease =
        acquire_repo_write_lease_with_wait(state, &repo, Duration::from_secs(retry_after)).await;
    let lease = match lease {
        Ok(lease) => lease,
        Err(error) => {
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::from_error(error, Some(retry_after)));
        }
    };

    let token = lease.token().to_string();
    let materializer =
        Materializer::with_lease_token(Arc::clone(&state.domain), token).using_upstream_auth(&auth);
    let result = materializer.materialize(request).await;
    let release_result = lease.release().await;

    match &result {
        Ok(response) => info!(
            repo = %response.repo,
            commit = %response.commit,
            source = ?response.source,
            elapsed_ms = elapsed_ms(domain_started),
            "domain materialize finished"
        ),
        Err(error) => info!(
            error = %error,
            elapsed_ms = elapsed_ms(domain_started),
            "domain materialize failed"
        ),
    }

    match (result, release_result) {
        (Ok(response), Ok(())) => Ok(Json(response).into_response()),
        (Err(error), Ok(())) => {
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            Err(error.into())
        }
        (Ok(_), Err(error)) => {
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            Err(ApiError::from_error(error, Some(retry_after)))
        }
        (Err(error), Err(release_error)) => {
            warn!(%repo, %release_error, "failed to release repo lease after materialize error");
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            Err(error.into())
        }
    }
}

async fn handle_resolve_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let request_id = state.next_request_id();
    let repo = request.repo.clone();
    let selector = format!("{:?}", request.selector);
    let span = info_span!(
        "api_request",
        request_id,
        endpoint = "resolve",
        repo = %repo
    );
    async move { handle_resolve_request_inner(state, request, auth, request_id, selector).await }
        .instrument(span)
        .await
}

async fn handle_resolve_request_inner(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
    request_id: u64,
    selector: String,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let repo = request.repo.clone();
    info!(
        request_id,
        repo = %repo,
        selector,
        auth = auth_label(&auth),
        "resolve request started"
    );
    if !state.rate_limiter.check() {
        state
            .metrics
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
        info!(
            request_id,
            repo = %repo,
            elapsed_ms = elapsed_ms(started),
            status = %StatusCode::TOO_MANY_REQUESTS,
            "resolve request finished"
        );
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limit exceeded".into(),
            retry_after: None,
        });
    }

    state
        .metrics
        .materialize_total
        .fetch_add(1, Ordering::Relaxed);

    let CheckedMaterializeRequest { request, auth } =
        match check_materialize_upstream_auth(request, auth).await {
            Ok(checked) => checked,
            Err(error) => {
                state
                    .metrics
                    .materialize_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                info!(
                    request_id,
                    repo = %repo,
                    elapsed_ms = elapsed_ms(started),
                    status = %error.status,
                    "resolve request finished"
                );
                return Err(error);
            }
        };

    let materializer = Materializer::new(Arc::clone(&state.domain)).using_upstream_auth(&auth);
    let domain_started = Instant::now();
    match materializer.resolve(request).await {
        Ok(response) => {
            info!(
                request_id,
                repo = %response.repo,
                commit = %response.commit,
                source = ?response.source,
                cache_available = response.cache_available,
                domain_elapsed_ms = elapsed_ms(domain_started),
                elapsed_ms = elapsed_ms(started),
                status = %StatusCode::OK,
                "resolve request finished"
            );
            Ok(Json(response).into_response())
        }
        Err(error) => {
            state
                .metrics
                .materialize_errors_total
                .fetch_add(1, Ordering::Relaxed);
            let error = ApiError::from(error);
            info!(
                request_id,
                repo = %repo,
                elapsed_ms = elapsed_ms(started),
                status = %error.status,
                "resolve request finished"
            );
            Err(error)
        }
    }
}

struct CheckedMaterializeRequest {
    request: MaterializeRequest,
    auth: UpstreamAuth,
}

async fn check_materialize_upstream_auth(
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<CheckedMaterializeRequest, ApiError> {
    if request.requires_upstream_auth() && !auth.is_authenticated() {
        return Err(
            GitCacheError::Unauthorized("upstream authorization is required".into()).into(),
        );
    }
    Ok(CheckedMaterializeRequest { request, auth })
}

/// Direct Git remote handler: `/git/{host}/{owner}/{repo}.git/...`
///
/// This is the read-through handler that makes the cache behave like a normal
/// Git remote. No prior `/v1/materialize` call is needed.
async fn git_repo(
    State(state): State<Arc<ApiState>>,
    Path(repo_path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let request_id = state.next_request_id();
    let path = uri.path().to_string();
    let method_for_span = method.clone();
    let request = GitRepoRequest {
        repo_path,
        query,
        headers,
        method,
        uri,
        body,
        request_id,
    };
    let span = info_span!(
        "api_request",
        request_id,
        endpoint = "direct_git",
        method = %method_for_span,
        path = %path
    );
    async move { git_repo_inner(state, request).await }
        .instrument(span)
        .await
}

struct GitRepoRequest {
    repo_path: String,
    query: HashMap<String, String>,
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    body: Bytes,
    request_id: u64,
}

async fn git_repo_inner(state: Arc<ApiState>, request: GitRepoRequest) -> Response {
    let GitRepoRequest {
        repo_path,
        query,
        headers,
        method,
        uri,
        body,
        request_id,
    } = request;
    let path = uri.path();

    // Reject git-receive-pack (push) requests.
    if path.contains("git-receive-pack")
        || query
            .get("service")
            .is_some_and(|s| s == "git-receive-pack")
    {
        return ApiError::from(GitCacheError::Unsupported(
            "git-receive-pack is disabled".into(),
        ))
        .into_response();
    }

    let repo = match repo_from_git_path(&repo_path) {
        Ok(repo) => repo,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let materializer = Materializer::new(Arc::clone(&state.domain));

    if let Err(error) = materializer.validate_host(&repo) {
        return ApiError::from(error).into_response();
    }

    if method == Method::GET
        && path.ends_with("/info/refs")
        && query.get("service").is_some_and(|s| s == "git-upload-pack")
    {
        let started = Instant::now();
        let auth = match direct_git_upstream_auth(&headers) {
            Ok(auth) => auth,
            Err(error) => return error.into_response(),
        };
        info!(
            request_id,
            repo = %repo,
            auth = auth_label(&auth),
            "direct git ref advertisement started"
        );
        state
            .metrics
            .git_remote_refs_total
            .fetch_add(1, Ordering::Relaxed);
        let materializer = materializer.using_upstream_auth(&auth);

        // Fetch upstream refs via ls-remote and synthesize the pkt-line
        // response directly. No objects are fetched here; the repo may not
        // even exist locally yet. The advertisement is a short-lived
        // repo-access proof for the matching upload-pack POST. The POST path
        // then performs the normal read-through availability work using the
        // same request auth.
        let refs_started = Instant::now();
        let comparison = match materializer.upstream_refs(&repo).await {
            Ok(c) => c,
            Err(error) => return ApiError::from(error).into_response(),
        };
        info!(
            request_id,
            repo = %repo,
            refs_count = comparison.all_upstream.len(),
            elapsed_ms = elapsed_ms(refs_started),
            "direct git upstream refs fetched"
        );
        state
            .direct_git_proofs
            .insert(&repo, &auth, comparison.clone());

        let output = synthesize_ref_advertisement(&comparison);
        let response = git_response(
            "application/x-git-upload-pack-advertisement",
            frame_ref_advertisement(&output),
        );
        info!(
            request_id,
            repo = %repo,
            auth = auth_label(&auth),
            elapsed_ms = elapsed_ms(started),
            status = %StatusCode::OK,
            "direct git ref advertisement finished"
        );
        response
    } else if method == Method::POST && path.ends_with("/git-upload-pack") {
        let started = Instant::now();
        let auth = match direct_git_upstream_auth(&headers) {
            Ok(auth) => auth,
            Err(error) => return error.into_response(),
        };
        info!(
            request_id,
            repo = %repo,
            auth = auth_label(&auth),
            body_bytes = body.len(),
            "direct git upload-pack started"
        );
        state
            .metrics
            .git_remote_upload_pack_total
            .fetch_add(1, Ordering::Relaxed);
        let cached_proof = state.direct_git_proofs.get(&repo, &auth);
        let (auth, comparison) = match cached_proof {
            Some((auth, comparison)) => {
                info!(
                    request_id,
                    repo = %repo,
                    auth = auth_label(&auth),
                    "direct git upload-pack using cached repo proof"
                );
                (auth, Some(comparison))
            }
            None => {
                // Direct Git POSTs can arrive without the matching GET, so
                // fall back to the same lightweight ref proof used by GET.
                // This proves repo access without fetching packs. The
                // materializer then performs the same read-through
                // availability work as main, using the request-scoped auth.
                let auth_started = Instant::now();
                let proof_materializer = materializer.using_upstream_auth(&auth);
                let comparison = match proof_materializer.upstream_refs(&repo).await {
                    Ok(comparison) => comparison,
                    Err(error) => return ApiError::from(error).into_response(),
                };
                info!(
                    request_id,
                    repo = %repo,
                    auth = auth_label(&auth),
                    refs_count = comparison.all_upstream.len(),
                    elapsed_ms = elapsed_ms(auth_started),
                    "direct git upload-pack repo access proved"
                );
                state
                    .direct_git_proofs
                    .insert(&repo, &auth, comparison.clone());
                (auth, Some(comparison))
            }
        };
        if parse_want_lines(&body).is_empty() {
            let materializer = materializer.using_upstream_auth(&auth);
            return match Box::pin(materializer.handle_upload_pack(
                &repo,
                &body,
                comparison.as_ref(),
            ))
            .await
            {
                Ok(process) => {
                    info!(
                        request_id,
                        repo = %repo,
                        auth = auth_label(&auth),
                        elapsed_ms = elapsed_ms(started),
                        status = %StatusCode::OK,
                        "direct git upload-pack process spawned"
                    );
                    stream_upload_pack_response(&state, process)
                }
                Err(error) => {
                    let error = ApiError::from(error);
                    info!(
                        request_id,
                        repo = %repo,
                        auth = auth_label(&auth),
                        elapsed_ms = elapsed_ms(started),
                        status = %error.status,
                        "direct git upload-pack failed"
                    );
                    error.into_response()
                }
            };
        }

        let lease = match acquire_repo_write_lease_for_git_client(&state, &repo).await {
            Ok(lease) => lease,
            Err(error) => {
                return ApiError::from_error(
                    error,
                    Some(state.domain.config.leases.busy_retry_after_seconds),
                )
                .into_response()
            }
        };
        let token = lease.token().to_string();
        let leased_materializer = Materializer::with_lease_token(Arc::clone(&state.domain), token)
            .using_upstream_auth(&auth);
        let process_result =
            Box::pin(leased_materializer.handle_upload_pack(&repo, &body, comparison.as_ref()))
                .await;
        let release_result = lease.release().await;

        match (process_result, release_result) {
            (Ok(process), Ok(())) => {
                info!(
                    request_id,
                    repo = %repo,
                    auth = auth_label(&auth),
                    elapsed_ms = elapsed_ms(started),
                    status = %StatusCode::OK,
                    "direct git upload-pack process spawned"
                );
                stream_upload_pack_response(&state, process)
            }
            (Ok(process), Err(error)) => {
                warn!(%repo, %error, "failed to release repo lease after upload-pack");
                stream_upload_pack_response(&state, process)
            }
            (Err(error), Ok(())) => {
                let error = ApiError::from(error);
                info!(
                    request_id,
                    repo = %repo,
                    auth = auth_label(&auth),
                    elapsed_ms = elapsed_ms(started),
                    status = %error.status,
                    "direct git upload-pack failed"
                );
                error.into_response()
            }
            (Err(error), Err(release_error)) => {
                warn!(%repo, %release_error, "failed to release repo lease after upload-pack error");
                ApiError::from(error).into_response()
            }
        }
    } else {
        ApiError::from(GitCacheError::Unsupported(format!(
            "unsupported git request: {method} {path}"
        )))
        .into_response()
    }
}

fn upstream_api_auth(headers: &HeaderMap) -> Result<UpstreamAuth, ApiError> {
    parse_optional_upstream_auth_header(headers, "git-cache-upstream-authorization")
}

fn direct_git_upstream_auth(headers: &HeaderMap) -> Result<UpstreamAuth, ApiError> {
    parse_optional_upstream_auth_header(headers, header::AUTHORIZATION.as_str())
}

fn parse_optional_upstream_auth_header(
    headers: &HeaderMap,
    name: &str,
) -> Result<UpstreamAuth, ApiError> {
    let Some(value) = headers.get(name) else {
        return Ok(UpstreamAuth::Anonymous);
    };
    let value = value.to_str().map_err(|_| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: "upstream authorization header must be valid ASCII".into(),
        retry_after: None,
    })?;
    UpstreamAuth::parse_header(value).map_err(|error| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: error.to_string(),
        retry_after: None,
    })
}

fn stream_upload_pack_response(state: &Arc<ApiState>, mut process: UploadPackProcess) -> Response {
    let timeout_duration = process.timeout();
    let permit = process.take_permit();
    let reader_stream =
        ReaderStream::with_capacity(process.stdout, GIT_UPLOAD_PACK_STREAM_BUFFER_BYTES);
    let max_bytes = state.domain.config.max_git_output_bytes as u64;
    let guarded = ChildGuardStream {
        inner: reader_stream,
        child: process.child,
        bytes_sent: 0,
        max_bytes,
        timeout: Box::pin(tokio::time::sleep(timeout_duration)),
        timeout_duration,
        timed_out: false,
        _permit: permit,
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(guarded))
        .expect("git upload-pack response")
}

fn git_response(content_type: &'static str, output: Vec<u8>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from(output))
        .expect("git response")
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    checked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Default)]
struct Metrics {
    request_ids: AtomicU64,
    materialize_total: AtomicU64,
    materialize_errors_total: AtomicU64,
    rate_limited_total: AtomicU64,
    git_remote_refs_total: AtomicU64,
    git_remote_upload_pack_total: AtomicU64,
}

fn elapsed_ms(started: Instant) -> u128 {
    started.elapsed().as_millis()
}

fn auth_label(auth: &UpstreamAuth) -> &'static str {
    if auth.is_authenticated() {
        "authenticated"
    } else {
        "anonymous"
    }
}

struct RateLimiter {
    limit: u32,
    state: Mutex<RateLimitWindow>,
}

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

        let Ok(mut state) = self.state.lock() else {
            return false;
        };
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
    retry_after: Option<u64>,
}

impl From<GitCacheError> for ApiError {
    fn from(error: GitCacheError) -> Self {
        Self::from_error(error, None)
    }
}

impl ApiError {
    fn from_error(error: GitCacheError, lease_busy_retry_after: Option<u64>) -> Self {
        let status = match error {
            GitCacheError::NotFound(_) => StatusCode::NOT_FOUND,
            GitCacheError::UpstreamUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            GitCacheError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            GitCacheError::Forbidden(_) => StatusCode::FORBIDDEN,
            GitCacheError::DiskFull(_) => StatusCode::INSUFFICIENT_STORAGE,
            GitCacheError::Unsupported(_) => StatusCode::METHOD_NOT_ALLOWED,
            GitCacheError::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            GitCacheError::Validation(_) => StatusCode::BAD_REQUEST,
            GitCacheError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            GitCacheError::Conflict(_) => StatusCode::CONFLICT,
            GitCacheError::LeaseBusy(_) => StatusCode::SERVICE_UNAVAILABLE,
            GitCacheError::LeaseLost(_)
            | GitCacheError::LeaseStealConflict(_)
            | GitCacheError::CasConflict(_) => StatusCode::CONFLICT,
            GitCacheError::PendingGenerationInvalid(_) => StatusCode::CONFLICT,
            GitCacheError::ColdHydrationFailed(_) => StatusCode::SERVICE_UNAVAILABLE,
            GitCacheError::Internal(_) | GitCacheError::Io(_) | GitCacheError::Json(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        let retry_after = match error {
            GitCacheError::LeaseBusy(_) => Some(lease_busy_retry_after.unwrap_or(1)),
            _ => None,
        };

        Self {
            status,
            message: error.to_string(),
            retry_after,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody<'a> {
            error: &'a str,
        }

        let mut response = (
            self.status,
            Json(ErrorBody {
                error: &self.message,
            }),
        )
            .into_response();
        if let Some(retry_after) = self.retry_after {
            if let Ok(value) = retry_after.to_string().parse() {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

pub fn empty_body() -> Body {
    Body::empty()
}

/// Wraps a `ReaderStream` and holds a child process handle to keep the process
/// alive for the duration of the HTTP response body stream. Also holds the
/// semaphore permit so it is not released until the stream is fully consumed.
struct ChildGuardStream<R: AsyncRead + Unpin> {
    inner: ReaderStream<R>,
    child: tokio::process::Child,
    bytes_sent: u64,
    max_bytes: u64,
    timeout: Pin<Box<Sleep>>,
    timeout_duration: Duration,
    timed_out: bool,
    _permit: Option<OwnedSemaphorePermit>,
}

impl<R: AsyncRead + Unpin> Stream for ChildGuardStream<R> {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.timed_out {
            return Poll::Ready(None);
        }

        if this.timeout.as_mut().poll(cx).is_ready() {
            this.timed_out = true;
            let _ = this.child.start_kill();
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "git upload-pack response exceeded timeout of {:?}",
                    this.timeout_duration
                ),
            ))));
        }

        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes_sent = this.bytes_sent.saturating_add(chunk.len() as u64);
                if this.bytes_sent > this.max_bytes {
                    Poll::Ready(Some(Err(std::io::Error::other(format!(
                        "git upload-pack response exceeded {} byte limit",
                        this.max_bytes
                    )))))
                } else {
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use git_cache_core::{
        MaterializeRequest, ObjectStoreConfig, RepoKey, Selector, UpstreamAuthorizationMode,
    };
    use git_cache_domain::parse_want_lines;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::process::Stdio;
    use tempfile::TempDir;
    use tokio::io::duplex;
    use tokio::process::Command;

    #[test]
    fn rate_limiter_blocks_after_limit() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check());
    }

    #[test]
    fn direct_git_proof_cache_does_not_downshift_basic_auth_to_public_proof() {
        let cache = DirectGitProofCache::new(Duration::from_secs(30));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let auth = UpstreamAuth::parse_header("Basic dXNlcjp0b2tlbg==").unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".into()),
            all_upstream: HashMap::from([("main".into(), "a".repeat(40))]),
        };

        cache.insert(&repo, &UpstreamAuth::Anonymous, comparison.clone());

        assert!(
            cache.get(&repo, &auth).is_none(),
            "token-present POSTs must use token-scoped proof"
        );
        let (effective_auth, cached) = cache
            .get(&repo, &UpstreamAuth::Anonymous)
            .expect("anonymous POST should reuse anonymous proof");
        assert_eq!(effective_auth, UpstreamAuth::Anonymous);
        assert_eq!(cached.default_branch, comparison.default_branch);
        assert_eq!(cached.all_upstream, comparison.all_upstream);
    }

    #[test]
    fn direct_git_proof_cache_keeps_authenticated_proof_scoped() {
        let cache = DirectGitProofCache::new(Duration::from_secs(30));
        let repo = RepoKey::parse("github.com/org/private").unwrap();
        let auth = UpstreamAuth::parse_header("Basic dXNlcjp0b2tlbg==").unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".into()),
            all_upstream: HashMap::from([("main".into(), "b".repeat(40))]),
        };

        cache.insert(&repo, &auth, comparison.clone());

        assert!(
            cache.get(&repo, &UpstreamAuth::Anonymous).is_none(),
            "authenticated proof must not satisfy anonymous POSTs"
        );
        let (effective_auth, cached) = cache
            .get(&repo, &auth)
            .expect("same Basic auth should reuse proof");
        assert_eq!(effective_auth, auth);
        assert_eq!(cached.all_upstream, comparison.all_upstream);
    }

    #[test]
    fn direct_git_proof_cache_expires_entries() {
        let cache = DirectGitProofCache::new(Duration::from_millis(1));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: None,
            all_upstream: HashMap::from([("main".into(), "c".repeat(40))]),
        };

        cache.insert(&repo, &UpstreamAuth::Anonymous, comparison);
        std::thread::sleep(Duration::from_millis(2));

        assert!(cache.get(&repo, &UpstreamAuth::Anonymous).is_none());
    }

    #[test]
    fn upstream_api_auth_ignores_gateway_bearer_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer gateway-token".parse().unwrap(),
        );

        let auth = upstream_api_auth(&headers).unwrap();

        assert_eq!(auth, UpstreamAuth::Anonymous);
    }

    #[tokio::test]
    async fn authenticated_resolve_is_rate_limited_before_upstream_work() {
        let tmp = TempDir::new().unwrap();
        let config = AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            public_base_url: "http://127.0.0.1:0".into(),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 60,
            max_git_output_bytes: 16 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 1,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            max_concurrent_generation_verifications: 1,
            leases: Default::default(),
        };
        let state = Arc::new(ApiState::try_new(config).unwrap());
        assert!(state.rate_limiter.check(), "first request consumes quota");

        let request = MaterializeRequest {
            repo: RepoKey::parse("evil.com/org/repo").unwrap(),
            selector: Selector::DefaultBranch,
            mode: Default::default(),
            upstream_authorization: UpstreamAuthorizationMode::Required,
        };
        let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();

        let result = handle_resolve_request(&state, request, auth).await;

        match result {
            Err(error) => assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS),
            Ok(_) => panic!("rate-limited authenticated resolve should not succeed"),
        }
    }

    #[tokio::test]
    async fn upload_pack_stream_times_out_when_reader_stays_pending() {
        let (reader, _writer) = duplex(64);
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep child");
        let timeout_duration = Duration::from_millis(10);
        let mut stream = ChildGuardStream {
            inner: ReaderStream::new(reader),
            child,
            bytes_sent: 0,
            max_bytes: 1024,
            timeout: Box::pin(tokio::time::sleep(timeout_duration)),
            timeout_duration,
            timed_out: false,
            _permit: None,
        };

        let item = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("stream should wake on timeout")
            .expect("stream should yield timeout error");
        let error = item.expect_err("silent stream should time out");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            error
                .to_string()
                .contains("git upload-pack response exceeded timeout"),
            "unexpected timeout error: {error}"
        );
    }

    // ── parse_want_lines contract tests ─────────────────────────────────

    fn make_pkt_line(data: &str) -> Vec<u8> {
        let len = 4 + data.len();
        format!("{len:04x}{data}").into_bytes()
    }

    #[test]
    fn parse_want_standard_line() {
        let sha = "a".repeat(40);
        let line = format!("want {sha}\n");
        let body = make_pkt_line(&line);
        let wants = parse_want_lines(&body);
        assert_eq!(wants, vec![sha]);
    }

    #[test]
    fn parse_want_with_capabilities() {
        let sha = "b".repeat(40);
        let line = format!("want {sha} multi_ack thin-pack\n");
        let body = make_pkt_line(&line);
        let wants = parse_want_lines(&body);
        assert_eq!(wants, vec![sha]);
    }

    #[test]
    fn parse_want_multiple_wants() {
        let sha1 = "a".repeat(40);
        let sha2 = "b".repeat(40);
        let mut body = make_pkt_line(&format!("want {sha1}\n"));
        body.extend(make_pkt_line(&format!("want {sha2}\n")));
        body.extend(b"0000");
        body.extend(b"0009done\n");
        let wants = parse_want_lines(&body);
        assert_eq!(wants, vec![sha1, sha2]);
    }

    #[test]
    fn parse_want_flush_in_middle_is_skipped() {
        let sha1 = "a".repeat(40);
        let sha2 = "c".repeat(40);
        let mut body = make_pkt_line(&format!("want {sha1}\n"));
        body.extend(b"0000");
        body.extend(make_pkt_line(&format!("want {sha2}\n")));
        let wants = parse_want_lines(&body);
        assert_eq!(wants, vec![sha1, sha2]);
    }

    #[test]
    fn parse_want_invalid_pkt_length_stops_gracefully() {
        let body = b"zzzzbogus data here";
        let wants = parse_want_lines(body);
        assert!(wants.is_empty());
    }

    #[test]
    fn parse_want_empty_body() {
        let wants = parse_want_lines(b"");
        assert!(wants.is_empty());
    }

    #[test]
    fn parse_want_non_want_lines_ignored() {
        let mut body = make_pkt_line("have aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
        body.extend(make_pkt_line("done\n"));
        let wants = parse_want_lines(&body);
        assert!(wants.is_empty());
    }

    #[test]
    fn parse_want_truncated_packet_stops_gracefully() {
        // Length says 50 bytes but body is shorter.
        let body = b"0032want short\n";
        let wants = parse_want_lines(body);
        assert!(wants.is_empty());
    }
}
