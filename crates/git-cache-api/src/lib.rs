use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::Stream;
use git_cache_core::{
    AppConfig, CommitSha, GitCacheError, MaterializeRequest, RepoKey, Result as CoreResult,
    Selector, UpstreamAuth, UpstreamAuthorizationMode,
};
use git_cache_disk::{AsyncDiskManager, AsyncReservation};
use git_cache_domain::materializer::repo_from_git_path;
pub use git_cache_domain::AppState as DomainAppState;
use git_cache_domain::{
    frame_ref_advertisement, plan_upload_pack_tee, synthesize_ref_advertisement, upload_pack_wants,
    AppState, Materializer, PackDemux, UpstreamRefComparison,
};
use git_cache_git::UploadPackProcess;
use http::{header, HeaderMap, Method, StatusCode, Uri};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;
use tokio_util::io::ReaderStream;
use tracing::{info, info_span, warn, Instrument};

const GIT_UPLOAD_PACK_STREAM_BUFFER_BYTES: usize = 64 * 1024;
const DIRECT_GIT_PROOF_TTL: Duration = Duration::from_secs(30);
const PROXY_ON_MISS_HEADER: &str = "git-cache-use-proxy-on-miss";
/// Disk reservation granularity for spooling a proxied upload-pack response
/// whose final size is unknown up front.
const TEE_SPOOL_RESERVE_CHUNK_BYTES: u64 = 256 * 1024 * 1024;
/// Bounded queue between the proxy stream and the spool writer; a full queue
/// (slow disk) abandons the tee rather than stalling the client stream.
const TEE_SPOOL_CHANNEL_CAPACITY: usize = 256;

pub fn app(config: AppConfig) -> Router {
    app_result(config).expect("failed to initialize git-cache-api")
}

pub fn app_result(config: AppConfig) -> CoreResult<Router> {
    let git_remote_enabled = config.git_remote.enabled;
    let state = Arc::new(ApiState::try_new(config)?);
    router(git_remote_enabled, state)
}

pub async fn app_result_async(config: AppConfig) -> CoreResult<Router> {
    Ok(app_with_shutdown_async(config).await?.0)
}

/// Like [`app_result_async`], but also returns a [`ReadinessGate`] that the
/// caller can flip during shutdown so `/healthz` starts failing and load
/// balancers stop routing new traffic while in-flight requests drain.
pub async fn app_with_shutdown_async(config: AppConfig) -> CoreResult<(Router, ReadinessGate)> {
    let git_remote_enabled = config.git_remote.enabled;
    let state = Arc::new(ApiState::try_new_async(config).await?);
    let gate = ReadinessGate(Arc::clone(&state.shutting_down));
    Ok((router(git_remote_enabled, state)?, gate))
}

/// Handle that marks the server as shutting down; once flipped, `/healthz`
/// returns 503 so orchestrators stop sending new traffic.
#[derive(Clone, Debug)]
pub struct ReadinessGate(Arc<AtomicBool>);

impl ReadinessGate {
    pub fn begin_shutdown(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
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
    direct_git_background_imports: Arc<Semaphore>,
    async_materialize_jobs: Arc<AsyncMaterializeJobs>,
    upstream_http: reqwest::Client,
    metrics: Arc<Metrics>,
    rate_limiter: Arc<RateLimiter>,
    shutting_down: Arc<AtomicBool>,
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
        spawn_repo_access_flusher(&domain);
        let upstream_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(domain.config.git_timeout_seconds))
            .build()
            .map_err(|err| {
                GitCacheError::Internal(format!("failed to build HTTP client: {err}"))
            })?;
        let background_import_concurrency = domain
            .config
            .git_remote
            .background_import_concurrency
            .max(1);
        let async_materialize_concurrency = domain.config.async_materialize_concurrency.max(1);
        Ok(Self {
            domain,
            direct_git_proofs: Arc::new(DirectGitProofCache::new(DIRECT_GIT_PROOF_TTL)),
            direct_git_background_imports: Arc::new(Semaphore::new(background_import_concurrency)),
            async_materialize_jobs: Arc::new(AsyncMaterializeJobs::new(
                async_materialize_concurrency,
            )),
            upstream_http,
            metrics: Arc::new(Metrics::default()),
            rate_limiter: Arc::new(rate_limiter),
            shutting_down: Arc::new(AtomicBool::new(false)),
        })
    }

    fn next_request_id(&self) -> u64 {
        self.metrics.request_ids.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Periodically flush buffered in-memory repo access timestamps to the
/// persistent disk index. The task holds only a weak reference to the app
/// state and exits once the application has been dropped. Spawning is skipped
/// when no tokio runtime is available (the next process start rebuilds
/// recency from the persisted index, so a missed flush only loses at most one
/// interval of recency).
fn spawn_repo_access_flusher(domain: &Arc<AppState>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let interval = Duration::from_secs(domain.config.disk.access_flush_interval_secs.max(1));
    let weak = Arc::downgrade(domain);
    handle.spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let Some(domain) = weak.upgrade() else {
                break;
            };
            if let Err(err) = domain.disk.flush_repo_accesses().await {
                warn!(error = %err, "failed to flush repo access timestamps");
            }
        }
    });
}

/// Serve the API on `listener` until SIGTERM/SIGINT, then drain gracefully:
/// `/healthz` starts failing so load balancers stop routing new traffic, the
/// configured readiness propagation delay passes, the server stops accepting
/// connections and drains in-flight requests for up to the configured drain
/// timeout, and finally any buffered repo access timestamps are flushed.
pub async fn serve(listener: tokio::net::TcpListener, config: AppConfig) -> CoreResult<()> {
    let shutdown_config = config.shutdown.clone();
    let git_remote_enabled = config.git_remote.enabled;
    let state = Arc::new(ApiState::try_new_async(config).await?);
    let domain = state.domain.clone();
    let readiness = ReadinessGate(Arc::clone(&state.shutting_down));
    let app = router(git_remote_enabled, state)?;

    let readiness_delay = Duration::from_secs(shutdown_config.readiness_delay_seconds);
    let drain_timeout = Duration::from_secs(shutdown_config.drain_timeout_seconds);
    run_until_shutdown(
        listener,
        app,
        readiness,
        readiness_delay,
        drain_timeout,
        shutdown_signal(),
    )
    .await?;

    if let Err(err) = domain.disk.flush_repo_accesses().await {
        warn!(error = %err, "failed to flush repo access timestamps during shutdown");
    }
    Ok(())
}

/// Serve `app` on `listener` until `signal` resolves, then drain: fail
/// readiness, wait `readiness_delay`, stop accepting connections, and let
/// in-flight requests finish for at most `drain_timeout` before returning.
async fn run_until_shutdown(
    listener: tokio::net::TcpListener,
    app: Router,
    readiness: ReadinessGate,
    readiness_delay: Duration,
    drain_timeout: Duration,
    signal: impl std::future::Future<Output = ()> + Send + 'static,
) -> CoreResult<()> {
    let (drain_deadline_tx, drain_deadline_rx) = tokio::sync::oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful_shutdown(
        readiness,
        readiness_delay,
        drain_timeout,
        drain_deadline_tx,
        signal,
    ));

    tokio::select! {
        result = server => {
            result.map_err(|err| GitCacheError::Internal(format!("server error: {err}")))?;
        }
        _ = wait_for_drain_deadline(drain_deadline_rx, drain_timeout) => {
            warn!(
                drain_timeout_seconds = drain_timeout.as_secs(),
                "drain timeout elapsed with requests still in flight; exiting"
            );
        }
    }
    Ok(())
}

/// Resolves once the process should stop accepting new connections: after the
/// shutdown signal is received, readiness is failed, and the configured
/// readiness propagation delay has passed. Signals `drain_deadline_tx` so the
/// caller can bound the remaining in-flight drain.
async fn graceful_shutdown(
    readiness: ReadinessGate,
    readiness_delay: Duration,
    drain_timeout: Duration,
    drain_deadline_tx: tokio::sync::oneshot::Sender<()>,
    signal: impl std::future::Future<Output = ()>,
) {
    signal.await;
    readiness.begin_shutdown();
    info!(
        readiness_delay_seconds = readiness_delay.as_secs(),
        drain_timeout_seconds = drain_timeout.as_secs(),
        "shutdown signal received; failing readiness, then draining in-flight requests"
    );
    tokio::time::sleep(readiness_delay).await;
    let _ = drain_deadline_tx.send(());
}

async fn wait_for_drain_deadline(
    drain_started: tokio::sync::oneshot::Receiver<()>,
    drain_timeout: Duration,
) {
    if drain_started.await.is_err() {
        // Server finished before shutdown began; never force-exit.
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(drain_timeout).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!(%err, "failed to install SIGINT handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                warn!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
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

async fn healthz(State(state): State<Arc<ApiState>>) -> Response {
    let shutting_down = state.shutting_down.load(Ordering::SeqCst);
    let body = Json(HealthResponse {
        ok: !shutting_down,
        checked_at: chrono::Utc::now(),
    });
    if shutting_down {
        (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
    } else {
        body.into_response()
    }
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
    Query(query): Query<MaterializeQuery>,
    headers: HeaderMap,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let auth = upstream_api_auth(&headers)?;
    let endpoint = if query.r#async {
        MaterializeEndpoint::MaterializeAsync
    } else {
        MaterializeEndpoint::Materialize
    };
    handle_checked_materialize_request(&state, endpoint, request, auth).await
}

#[derive(Debug, Default, Deserialize)]
struct MaterializeQuery {
    #[serde(default, rename = "async")]
    r#async: bool,
}

async fn resolve(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let auth = upstream_api_auth(&headers)?;
    handle_checked_materialize_request(&state, MaterializeEndpoint::Resolve, request, auth).await
}

#[derive(Debug, Clone, Copy)]
enum MaterializeEndpoint {
    Materialize,
    MaterializeAsync,
    Resolve,
}

impl MaterializeEndpoint {
    fn name(self) -> &'static str {
        match self {
            Self::Materialize => "materialize",
            Self::MaterializeAsync => "materialize_async",
            Self::Resolve => "resolve",
        }
    }
}

async fn handle_checked_materialize_request(
    state: &Arc<ApiState>,
    endpoint: MaterializeEndpoint,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let request_id = state.next_request_id();
    let repo = request.repo.clone();
    let selector = format!("{:?}", request.selector);
    let span = info_span!(
        "api_request",
        request_id,
        endpoint = endpoint.name(),
        repo = %repo
    );
    async move {
        handle_checked_materialize_request_inner(
            state, endpoint, request, auth, request_id, selector,
        )
        .await
    }
    .instrument(span)
    .await
}

async fn handle_checked_materialize_request_inner(
    state: &Arc<ApiState>,
    endpoint: MaterializeEndpoint,
    request: MaterializeRequest,
    auth: UpstreamAuth,
    request_id: u64,
    selector: String,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let repo = request.repo.clone();
    info!(
        request_id,
        endpoint = endpoint.name(),
        repo = %repo,
        selector,
        auth = auth_label(&auth),
        "api request started"
    );
    if !state.rate_limiter.check() {
        state
            .metrics
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
        info!(
            request_id,
            endpoint = endpoint.name(),
            repo = %repo,
            elapsed_ms = elapsed_ms(started),
            status = %StatusCode::TOO_MANY_REQUESTS,
            "api request finished"
        );
        return Err(ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: "rate limit exceeded".into(),
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
                    endpoint = endpoint.name(),
                    repo = %repo,
                    elapsed_ms = elapsed_ms(started),
                    status = %error.status,
                    "api request finished"
                );
                return Err(error);
            }
        };

    let result = run_domain_request(state, endpoint, request, auth).await;
    match &result {
        Ok(response) => info!(
            request_id,
            endpoint = endpoint.name(),
            elapsed_ms = elapsed_ms(started),
            status = %response.status(),
            "api request finished"
        ),
        Err(error) => info!(
            request_id,
            endpoint = endpoint.name(),
            elapsed_ms = elapsed_ms(started),
            status = %error.status,
            "api request finished"
        ),
    }
    result
}

async fn run_domain_request(
    state: &Arc<ApiState>,
    endpoint: MaterializeEndpoint,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let materializer = Materializer::new(Arc::clone(&state.domain)).using_upstream_auth(&auth);
    let domain_started = Instant::now();
    let result = match endpoint {
        MaterializeEndpoint::Materialize => {
            materializer.materialize(request).await.map(|response| {
                info!(
                    repo = %response.repo,
                    commit = %response.commit,
                    source = ?response.source,
                    elapsed_ms = elapsed_ms(domain_started),
                    "domain materialize finished"
                );
                Json(response).into_response()
            })
        }
        MaterializeEndpoint::MaterializeAsync => {
            materializer.resolve(request.clone()).await.map(|response| {
                if response.cache_available {
                    info!(
                        repo = %response.repo,
                        commit = %response.commit,
                        elapsed_ms = elapsed_ms(domain_started),
                        "async materialize already cached"
                    );
                    Json(response).into_response()
                } else {
                    let queued = state.async_materialize_jobs.spawn(
                        materializer.clone(),
                        request,
                        response.commit.clone(),
                    );
                    info!(
                        repo = %response.repo,
                        commit = %response.commit,
                        queued,
                        elapsed_ms = elapsed_ms(domain_started),
                        "async materialize accepted"
                    );
                    (StatusCode::ACCEPTED, Json(response)).into_response()
                }
            })
        }
        MaterializeEndpoint::Resolve => materializer.resolve(request).await.map(|response| {
            info!(
                repo = %response.repo,
                commit = %response.commit,
                source = ?response.source,
                cache_available = response.cache_available,
                elapsed_ms = elapsed_ms(domain_started),
                "domain resolve finished"
            );
            Json(response).into_response()
        }),
    };

    result.map_err(|error| {
        info!(
            endpoint = endpoint.name(),
            error = %error,
            elapsed_ms = elapsed_ms(domain_started),
            "domain request failed"
        );
        state
            .metrics
            .materialize_errors_total
            .fetch_add(1, Ordering::Relaxed);
        error.into()
    })
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
        let body =
            match decode_git_request_body(&headers, body, state.domain.config.max_git_output_bytes)
            {
                Ok(body) => body,
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
        let materializer = materializer.using_upstream_auth(&auth);

        if !proxy_on_miss_disabled(
            &headers,
            state.domain.config.git_remote.proxy_on_miss_by_default,
        ) {
            let local_check_started = Instant::now();
            let can_serve_locally = match materializer
                .prepare_upload_pack_from_cache(&repo, &body)
                .await
            {
                Ok(can_serve) => can_serve,
                Err(error) => return ApiError::from(error).into_response(),
            };
            info!(
                request_id,
                repo = %repo,
                auth = auth_label(&auth),
                can_serve_locally,
                elapsed_ms = elapsed_ms(local_check_started),
                "direct git proxy-on-miss cache readiness checked"
            );

            if !can_serve_locally {
                match proxy_upload_pack_to_upstream(UploadPackProxyRequest {
                    state: &state,
                    materializer: &materializer,
                    repo: &repo,
                    auth: &auth,
                    headers: &headers,
                    body: body.clone(),
                    comparison: comparison.clone(),
                    request_id,
                    request_started: started,
                })
                .await
                {
                    Ok(response) => return response,
                    Err(ProxyFallback::UseLocal) => {
                        info!(
                            request_id,
                            repo = %repo,
                            auth = auth_label(&auth),
                            "direct git proxy-on-miss unavailable; using local read-through"
                        );
                    }
                    Err(ProxyFallback::Error(error)) => return error.into_response(),
                }
            } else {
                info!(
                    request_id,
                    repo = %repo,
                    auth = auth_label(&auth),
                    "direct git proxy-on-miss cache hit; serving local upload-pack"
                );
            }
        }

        let result =
            Box::pin(materializer.handle_upload_pack(&repo, &body, comparison.as_ref())).await;

        match result {
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
    })?;
    UpstreamAuth::parse_header(value).map_err(|error| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: error.to_string(),
    })
}

enum ProxyFallback {
    UseLocal,
    Error(ApiError),
}

struct UploadPackProxyRequest<'a> {
    state: &'a Arc<ApiState>,
    materializer: &'a Materializer,
    repo: &'a RepoKey,
    auth: &'a UpstreamAuth,
    headers: &'a HeaderMap,
    body: Bytes,
    comparison: Option<UpstreamRefComparison>,
    request_id: u64,
    request_started: Instant,
}

async fn proxy_upload_pack_to_upstream(
    request: UploadPackProxyRequest<'_>,
) -> Result<Response, ProxyFallback> {
    let UploadPackProxyRequest {
        state,
        materializer,
        repo,
        auth,
        headers,
        body,
        comparison,
        request_id,
        request_started,
    } = request;
    let upstream_url = materializer
        .upstream_url(repo)
        .map_err(|error| ProxyFallback::Error(error.into()))?;
    let Some(upload_pack_url) = upload_pack_endpoint(&upstream_url) else {
        return Err(ProxyFallback::UseLocal);
    };

    let proxy_started = Instant::now();
    let mut request = state
        .upstream_http
        .post(upload_pack_url)
        .header(
            header::CONTENT_TYPE.as_str(),
            "application/x-git-upload-pack-request",
        )
        .header(header::CACHE_CONTROL.as_str(), "no-cache")
        .body(body.clone());
    if let Some(raw_auth) = auth.raw_header() {
        request = request.header(header::AUTHORIZATION.as_str(), raw_auth);
    }
    if let Some(value) = headers
        .get("Git-Protocol")
        .and_then(|value| value.to_str().ok())
    {
        request = request.header("Git-Protocol", value);
    }

    info!(
        request_id,
        repo = %repo,
        auth = auth_label(auth),
        "direct git cold-miss upstream proxy started"
    );
    let response = request.send().await.map_err(|error| {
        ProxyFallback::Error(
            GitCacheError::UpstreamUnavailable(format!(
                "upstream upload-pack proxy request failed: {error}"
            ))
            .into(),
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProxyFallback::Error(
            GitCacheError::UpstreamUnavailable(format!(
                "upstream upload-pack proxy returned HTTP {status}"
            ))
            .into(),
        ));
    }

    let tee = if state.domain.config.git_remote.proxy_tee_import {
        plan_upload_pack_tee(&body).map(|plan| {
            info!(
                request_id,
                repo = %repo,
                sideband = plan.sideband,
                blobless = plan.blobless,
                "direct git proxy tee import engaged"
            );
            let (sender, writer) = spawn_tee_spool_writer(state.domain.disk.clone(), request_id);
            ProxyTee {
                demux: PackDemux::new(plan.sideband),
                sender,
                writer,
            }
        })
    } else {
        None
    };
    let generation_task = direct_git_generation_task(
        state,
        materializer,
        repo,
        auth,
        &body,
        comparison.as_ref(),
        request_id,
    );
    let warm_task = DirectGitWarmTask {
        imports: Arc::clone(&state.direct_git_background_imports),
        materializer: materializer.clone(),
        repo: repo.clone(),
        body,
        comparison,
        request_id,
        generation_task,
    };
    let stream = UpstreamProxyStream {
        inner: Box::pin(response.bytes_stream()),
        warm_task: Some(warm_task),
        tee,
        bytes_sent: 0,
        max_bytes: state.domain.config.max_git_output_bytes as u64,
    };

    info!(
        request_id,
        repo = %repo,
        auth = auth_label(auth),
        proxy_setup_elapsed_ms = elapsed_ms(proxy_started),
        elapsed_ms = elapsed_ms(request_started),
        status = %StatusCode::OK,
        "direct git cold-miss upstream proxy streaming"
    );
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .expect("git upload-pack proxy response"))
}

fn upload_pack_endpoint(upstream_url: &str) -> Option<String> {
    if !(upstream_url.starts_with("https://") || upstream_url.starts_with("http://")) {
        return None;
    }
    Some(format!(
        "{}/git-upload-pack",
        upstream_url.trim_end_matches('/')
    ))
}

/// Whether the cold-miss upstream proxy is disabled for this request.
///
/// The `git-cache-use-proxy-on-miss` header overrides the configured default:
/// a falsey value (`0`, `false`, `no`, `off`) disables the proxy, any other
/// value enables it, and an absent header falls back to `default_enabled`.
fn proxy_on_miss_disabled(headers: &HeaderMap, default_enabled: bool) -> bool {
    let Some(value) = headers.get(PROXY_ON_MISS_HEADER) else {
        return !default_enabled;
    };
    let Ok(value) = value.to_str() else {
        return !default_enabled;
    };
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Decode a Git smart HTTP request body according to its `Content-Encoding`.
///
/// Git clients gzip upload-pack request bodies above a small threshold
/// (`Content-Encoding: gzip`), so the body must be inflated before pkt-line
/// parsing. Decompression is bounded by `max_bytes` to keep allocations
/// bounded; unknown encodings are rejected.
fn decode_git_request_body(
    headers: &HeaderMap,
    body: Bytes,
    max_bytes: usize,
) -> Result<Bytes, ApiError> {
    let encoding = match headers.get(header::CONTENT_ENCODING) {
        Some(value) => value,
        None => return Ok(body),
    };
    let encoding = encoding.to_str().map_err(|_| {
        ApiError::from(GitCacheError::Validation(
            "invalid content-encoding header".into(),
        ))
    })?;
    match encoding.trim().to_ascii_lowercase().as_str() {
        "" | "identity" => Ok(body),
        "gzip" | "x-gzip" => {
            use std::io::Read;
            let limit = max_bytes as u64;
            let mut decoder = flate2::bufread::GzDecoder::new(body.as_ref()).take(limit + 1);
            let mut decoded = Vec::new();
            decoder.read_to_end(&mut decoded).map_err(|error| {
                ApiError::from(GitCacheError::Validation(format!(
                    "invalid gzip request body: {error}"
                )))
            })?;
            if decoded.len() as u64 > limit {
                return Err(ApiError::from(GitCacheError::Validation(format!(
                    "gzip request body exceeded {max_bytes} byte limit"
                ))));
            }
            Ok(Bytes::from(decoded))
        }
        other => Err(ApiError::from(GitCacheError::Unsupported(format!(
            "unsupported content-encoding: {other}"
        )))),
    }
}

struct DirectGitWarmTask {
    imports: Arc<Semaphore>,
    materializer: Materializer,
    repo: RepoKey,
    body: Bytes,
    comparison: Option<UpstreamRefComparison>,
    request_id: u64,
    generation_task: Option<DirectGitGenerationTask>,
}

impl DirectGitWarmTask {
    fn spawn(self) {
        tokio::spawn(async move {
            let task_started = Instant::now();
            let body_bytes = self.body.len();
            let cached_ref_proof = self.comparison.is_some();
            let permit = match Arc::clone(&self.imports).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    warn!(
                        request_id = self.request_id,
                        repo = %self.repo,
                        "direct git proxy-on-miss cache warm semaphore closed"
                    );
                    return;
                }
            };
            let warm_started = Instant::now();
            info!(
                request_id = self.request_id,
                repo = %self.repo,
                body_bytes,
                cached_ref_proof,
                queue_elapsed_ms = elapsed_ms(task_started),
                "direct git proxy-on-miss cache warm started"
            );
            let result = Box::pin(self.materializer.warm_upload_pack(
                &self.repo,
                &self.body,
                self.comparison.as_ref(),
            ))
            .await;
            drop(permit);
            match &result {
                Ok(()) => info!(
                    request_id = self.request_id,
                    repo = %self.repo,
                    body_bytes,
                    cached_ref_proof,
                    warm_elapsed_ms = elapsed_ms(warm_started),
                    total_async_elapsed_ms = elapsed_ms(task_started),
                    "direct git proxy-on-miss cache warm finished"
                ),
                Err(error) => warn!(
                    request_id = self.request_id,
                    repo = %self.repo,
                    %error,
                    body_bytes,
                    cached_ref_proof,
                    warm_elapsed_ms = elapsed_ms(warm_started),
                    total_async_elapsed_ms = elapsed_ms(task_started),
                    "direct git proxy-on-miss cache warm failed"
                ),
            }
            if result.is_ok() {
                self.spawn_generation_materialize("warm");
            }
        });
    }

    fn spawn_generation_materialize(&self, trigger: &'static str) {
        if let Some(task) = &self.generation_task {
            task.spawn(trigger);
        }
    }
}

struct DirectGitGenerationTask {
    jobs: Arc<AsyncMaterializeJobs>,
    materializer: Materializer,
    request: MaterializeRequest,
    commit: CommitSha,
    request_id: u64,
}

impl DirectGitGenerationTask {
    fn spawn(&self, trigger: &'static str) {
        let queued = self.jobs.spawn(
            self.materializer.clone(),
            self.request.clone(),
            self.commit.clone(),
        );
        info!(
            request_id = self.request_id,
            repo = %self.request.repo,
            commit = %self.commit,
            trigger,
            queued,
            "direct git proxy-on-miss async materialize queued"
        );
    }
}

fn direct_git_generation_task(
    state: &Arc<ApiState>,
    materializer: &Materializer,
    repo: &RepoKey,
    auth: &UpstreamAuth,
    body: &[u8],
    comparison: Option<&UpstreamRefComparison>,
    request_id: u64,
) -> Option<DirectGitGenerationTask> {
    let comparison = comparison?;
    let wants = match upload_pack_wants(body) {
        Ok(wants) => wants,
        Err(error) => {
            warn!(
                request_id,
                repo = %repo,
                %error,
                "direct git proxy-on-miss async materialize skipped: upload-pack wants did not parse"
            );
            return None;
        }
    };
    let commit = wants
        .into_iter()
        .find(|want| comparison.branch_for_commit(want).is_some())?;
    let upstream_authorization = if auth.is_authenticated() {
        UpstreamAuthorizationMode::Required
    } else {
        UpstreamAuthorizationMode::Anonymous
    };
    Some(DirectGitGenerationTask {
        jobs: Arc::clone(&state.async_materialize_jobs),
        materializer: materializer.clone(),
        request: MaterializeRequest {
            repo: repo.clone(),
            selector: Selector::Commit(commit.clone()),
            upstream_authorization,
        },
        commit,
        request_id,
    })
}

enum TeeSpoolMsg {
    Chunk(Bytes),
    Done,
}

struct TeeSpoolOutput {
    path: std::path::PathBuf,
    sha256: String,
    bytes: u64,
    reservations: Vec<AsyncReservation>,
}

async fn release_reservations(reservations: Vec<AsyncReservation>) {
    for reservation in reservations {
        if let Err(error) = reservation.release().await {
            warn!(%error, "failed to release tee spool disk reservation");
        }
    }
}

/// Spawns the spool writer task that persists demuxed pack bytes to a
/// reserved temp file. Returns `Some(output)` only when a `Done` message was
/// received and all writes flushed; any other termination (sender dropped,
/// write/reservation failure) cleans up and returns `None`.
fn spawn_tee_spool_writer(
    disk: AsyncDiskManager,
    request_id: u64,
) -> (
    mpsc::Sender<TeeSpoolMsg>,
    tokio::task::JoinHandle<Option<TeeSpoolOutput>>,
) {
    let (sender, mut receiver) = mpsc::channel(TEE_SPOOL_CHANNEL_CAPACITY);
    let handle = tokio::spawn(async move {
        let mut reservations: Vec<AsyncReservation> = Vec::new();
        let result = async {
            let first = disk.reserve(TEE_SPOOL_RESERVE_CHUNK_BYTES).await?;
            let spool_dir = first.temp_path()?;
            reservations.push(first);
            tokio::fs::create_dir_all(&spool_dir).await?;
            let path = spool_dir.join("tee-spool.pack");
            let mut file = tokio::fs::File::create(&path).await?;
            let mut reserved = TEE_SPOOL_RESERVE_CHUNK_BYTES;
            let mut hasher = Sha256::new();
            let mut bytes: u64 = 0;
            while let Some(message) = receiver.recv().await {
                match message {
                    TeeSpoolMsg::Chunk(chunk) => {
                        bytes = bytes.saturating_add(chunk.len() as u64);
                        while bytes > reserved {
                            reservations.push(disk.reserve(TEE_SPOOL_RESERVE_CHUNK_BYTES).await?);
                            reserved += TEE_SPOOL_RESERVE_CHUNK_BYTES;
                        }
                        hasher.update(&chunk);
                        file.write_all(&chunk).await?;
                    }
                    TeeSpoolMsg::Done => {
                        file.flush().await?;
                        return Ok::<_, GitCacheError>(Some(TeeSpoolOutput {
                            path,
                            sha256: format!("{:x}", hasher.finalize()),
                            bytes,
                            reservations: Vec::new(),
                        }));
                    }
                }
            }
            // Sender dropped without `Done`: the tee was abandoned.
            Ok(None)
        }
        .await;
        match result {
            Ok(Some(mut output)) => {
                output.reservations = std::mem::take(&mut reservations);
                Some(output)
            }
            Ok(None) => {
                release_reservations(std::mem::take(&mut reservations)).await;
                None
            }
            Err(error) => {
                warn!(request_id, %error, "tee spool writer failed; abandoning tee import");
                release_reservations(std::mem::take(&mut reservations)).await;
                None
            }
        }
    });
    (sender, handle)
}

/// Per-response state for tee-importing a proxied upload-pack stream.
struct ProxyTee {
    demux: PackDemux,
    sender: mpsc::Sender<TeeSpoolMsg>,
    writer: tokio::task::JoinHandle<Option<TeeSpoolOutput>>,
}

/// Imports the spooled pack once the writer finishes; falls back to the
/// warm refetch on any failure so the repo still becomes servable.
fn spawn_tee_import(
    writer: tokio::task::JoinHandle<Option<TeeSpoolOutput>>,
    warm: DirectGitWarmTask,
) {
    tokio::spawn(async move {
        let output = match writer.await {
            Ok(Some(output)) => output,
            Ok(None) => {
                warm.spawn();
                return;
            }
            Err(error) => {
                warn!(request_id = warm.request_id, %error, "tee spool writer task panicked");
                warm.spawn();
                return;
            }
        };
        let imports = Arc::clone(&warm.imports);
        let permit = match imports.acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                warn!(
                    request_id = warm.request_id,
                    repo = %warm.repo,
                    "tee import semaphore closed"
                );
                release_reservations(output.reservations).await;
                return;
            }
        };
        let import_started = Instant::now();
        let result = warm
            .materializer
            .import_proxied_upload_pack(&warm.repo, &warm.body, &output.path, &output.sha256)
            .await;
        drop(permit);
        release_reservations(output.reservations).await;
        match result {
            Ok(()) => {
                info!(
                    request_id = warm.request_id,
                    repo = %warm.repo,
                    pack_bytes = output.bytes,
                    import_elapsed_ms = elapsed_ms(import_started),
                    "tee import of proxied upload-pack response finished"
                );
                warm.spawn_generation_materialize("tee_import");
            }
            Err(error) => {
                warn!(
                    request_id = warm.request_id,
                    repo = %warm.repo,
                    %error,
                    pack_bytes = output.bytes,
                    import_elapsed_ms = elapsed_ms(import_started),
                    "tee import failed; falling back to warm refetch"
                );
                warm.spawn();
            }
        }
    });
}

/// Bounded background runner for `/v1/materialize?async=true`. Jobs are
/// deduplicated per repo+commit; callers poll `/v1/resolve` for completion.
struct AsyncMaterializeJobs {
    permits: Arc<Semaphore>,
    inflight: Mutex<HashSet<(RepoKey, CommitSha)>>,
}

impl AsyncMaterializeJobs {
    fn new(concurrency: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(concurrency)),
            inflight: Mutex::new(HashSet::new()),
        }
    }

    /// Queues a background materialize for the resolved commit. Returns false
    /// when an equivalent job is already in flight.
    fn spawn(
        self: &Arc<Self>,
        materializer: Materializer,
        request: MaterializeRequest,
        commit: CommitSha,
    ) -> bool {
        let key = (request.repo.clone(), commit);
        {
            let Ok(mut inflight) = self.inflight.lock() else {
                warn!(
                    repo = %key.0,
                    commit = %key.1,
                    "async materialize in-flight lock poisoned"
                );
                return false;
            };
            if !inflight.insert(key.clone()) {
                return false;
            }
        }
        let jobs = Arc::clone(self);
        tokio::spawn(async move {
            let started = Instant::now();
            let result = match jobs.permits.acquire().await {
                Ok(_permit) => materializer.materialize(request).await.map(|_| ()),
                Err(_) => Err(GitCacheError::Internal(
                    "async materialize semaphore closed".into(),
                )),
            };
            match result {
                Ok(()) => info!(
                    repo = %key.0,
                    commit = %key.1,
                    elapsed_ms = elapsed_ms(started),
                    "async materialize finished"
                ),
                Err(error) => warn!(
                    repo = %key.0,
                    commit = %key.1,
                    %error,
                    elapsed_ms = elapsed_ms(started),
                    "async materialize failed"
                ),
            }
            if let Ok(mut inflight) = jobs.inflight.lock() {
                inflight.remove(&key);
            }
        });
        true
    }
}

type ReqwestBytesStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static>>;

struct UpstreamProxyStream {
    inner: ReqwestBytesStream,
    warm_task: Option<DirectGitWarmTask>,
    tee: Option<ProxyTee>,
    bytes_sent: u64,
    max_bytes: u64,
}

impl UpstreamProxyStream {
    /// Feed a proxied chunk into the tee demux/spool; abandons the tee on
    /// demux failure or spool backpressure (dropping the sender makes the
    /// writer task clean up).
    fn tee_chunk(&mut self, chunk: &Bytes) {
        let Some(tee) = self.tee.as_mut() else {
            return;
        };
        let mut pack = Vec::new();
        if tee.demux.feed(chunk, &mut pack).is_err() {
            self.tee = None;
            return;
        }
        if !pack.is_empty()
            && tee
                .sender
                .try_send(TeeSpoolMsg::Chunk(Bytes::from(pack)))
                .is_err()
        {
            warn!("tee spool backpressure or writer gone; abandoning tee import");
            self.tee = None;
        }
    }
}

impl Stream for UpstreamProxyStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes_sent = this.bytes_sent.saturating_add(chunk.len() as u64);
                if this.bytes_sent > this.max_bytes {
                    warn!(
                        bytes_sent = this.bytes_sent,
                        max_bytes = this.max_bytes,
                        "upstream upload-pack proxy response exceeded byte limit; aborting stream"
                    );
                    Poll::Ready(Some(Err(std::io::Error::other(format!(
                        "upstream upload-pack proxy response exceeded {} byte limit",
                        this.max_bytes
                    )))))
                } else {
                    this.tee_chunk(&chunk);
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(std::io::Error::other(
                format!("upstream upload-pack proxy stream failed: {error}"),
            )))),
            Poll::Ready(None) => {
                let mut tee_spawned = false;
                if let Some(tee) = this.tee.take() {
                    if tee.demux.pack_complete() && tee.sender.try_send(TeeSpoolMsg::Done).is_ok() {
                        if let Some(warm) = this.warm_task.take() {
                            spawn_tee_import(tee.writer, warm);
                            tee_spawned = true;
                        }
                    }
                }
                if !tee_spawned {
                    if let Some(task) = this.warm_task.take() {
                        task.spawn();
                    }
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The background warm must run regardless of how the proxied stream ends:
/// byte-limit aborts, upstream errors, and client disconnects all drop the
/// stream without reaching `Poll::Ready(None)`, and without the warm a repo
/// whose proxied response cannot complete would never become servable from
/// the local cache.
impl Drop for UpstreamProxyStream {
    fn drop(&mut self) {
        if let Some(task) = self.warm_task.take() {
            if tokio::runtime::Handle::try_current().is_ok() {
                task.spawn();
            }
        }
    }
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
}

impl From<GitCacheError> for ApiError {
    fn from(error: GitCacheError) -> Self {
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
            GitCacheError::Internal(_) | GitCacheError::Io(_) | GitCacheError::Json(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
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
            Poll::Ready(None) => {
                // upload-pack exiting nonzero after stdout closes (e.g.
                // pack-objects dying on missing blobs in a partially hydrated
                // repo) is invisible to the HTTP layer; surface it in logs.
                match this.child.try_wait() {
                    Ok(Some(status)) if !status.success() => warn!(
                        exit_code = status.code(),
                        bytes_sent = this.bytes_sent,
                        "git upload-pack exited with failure status after streaming response"
                    ),
                    _ => {}
                }
                Poll::Ready(None)
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
    use std::fs;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::process::{Command as StdCommand, Stdio};
    use tempfile::TempDir;
    use tokio::io::duplex;
    use tokio::process::Command;

    fn gzip_compress(data: &[u8]) -> Bytes {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        Bytes::from(encoder.finish().unwrap())
    }

    fn pkt_line(out: &mut Vec<u8>, data: &str) {
        let len = 4 + data.len();
        out.extend_from_slice(format!("{len:04x}").as_bytes());
        out.extend_from_slice(data.as_bytes());
    }

    fn upload_pack_body(lines: &[String]) -> Bytes {
        let mut out = Vec::new();
        for line in lines {
            pkt_line(&mut out, line);
        }
        out.extend_from_slice(b"0000");
        pkt_line(&mut out, "done\n");
        Bytes::from(out)
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = StdCommand::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = StdCommand::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn decode_git_request_body_passes_through_without_encoding() {
        let body = Bytes::from_static(b"0032want abc\n00000009done\n");
        let decoded = decode_git_request_body(&HeaderMap::new(), body.clone(), 1024).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn decode_git_request_body_inflates_gzip() {
        let plain = b"0032want abcdef0123456789\n00000009done\n";
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let decoded = decode_git_request_body(&headers, gzip_compress(plain), 1024).unwrap();
        assert_eq!(decoded.as_ref(), plain);
    }

    #[test]
    fn decode_git_request_body_bounds_inflated_size() {
        let plain = vec![b'a'; 4096];
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let error = decode_git_request_body(&headers, gzip_compress(&plain), 1024).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decode_git_request_body_rejects_invalid_gzip() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
        let error =
            decode_git_request_body(&headers, Bytes::from_static(b"not gzip"), 1024).unwrap_err();
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn decode_git_request_body_rejects_unknown_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_ENCODING, "br".parse().unwrap());
        let error =
            decode_git_request_body(&headers, Bytes::from_static(b"data"), 1024).unwrap_err();
        assert_eq!(error.status, StatusCode::METHOD_NOT_ALLOWED);
    }

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
    fn upload_pack_endpoint_is_only_for_http_origins() {
        assert_eq!(
            upload_pack_endpoint("https://github.com/org/repo.git").as_deref(),
            Some("https://github.com/org/repo.git/git-upload-pack")
        );
        assert_eq!(
            upload_pack_endpoint("http://git.example.com/org/repo.git/").as_deref(),
            Some("http://git.example.com/org/repo.git/git-upload-pack")
        );
        assert_eq!(upload_pack_endpoint("/tmp/upstreams/org/repo.git"), None);
    }

    #[test]
    fn proxy_on_miss_header_overrides_configured_default() {
        let mut headers = HeaderMap::new();
        assert!(proxy_on_miss_disabled(&headers, false));
        assert!(!proxy_on_miss_disabled(&headers, true));

        headers.insert(PROXY_ON_MISS_HEADER, "1".parse().unwrap());
        assert!(!proxy_on_miss_disabled(&headers, false));

        for opt_out in ["0", "false", "no", "off", " Off "] {
            headers.insert(PROXY_ON_MISS_HEADER, opt_out.parse().unwrap());
            assert!(
                proxy_on_miss_disabled(&headers, true),
                "{opt_out} should opt out"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_warm_task_queues_async_generation_materialize() {
        let tmp = TempDir::new().unwrap();
        let upstream_bare = tmp.path().join("upstreams/github.com/org/repo.git");
        let upstream_work = tmp.path().join("work");
        fs::create_dir_all(upstream_bare.parent().unwrap()).unwrap();
        fs::create_dir_all(&upstream_work).unwrap();

        run_git(
            tmp.path(),
            &["init", "--bare", upstream_bare.to_str().unwrap()],
        );
        run_git(&upstream_work, &["init"]);
        run_git(
            &upstream_work,
            &["config", "user.email", "test@example.com"],
        );
        run_git(&upstream_work, &["config", "user.name", "Test"]);
        fs::write(upstream_work.join("README.md"), "initial\n").unwrap();
        run_git(&upstream_work, &["add", "README.md"]);
        run_git(&upstream_work, &["commit", "-m", "initial"]);
        run_git(&upstream_work, &["branch", "-M", "main"]);
        run_git(
            &upstream_work,
            &["remote", "add", "origin", upstream_bare.to_str().unwrap()],
        );
        run_git(&upstream_work, &["push", "origin", "main"]);
        run_git(&upstream_bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let commit = CommitSha::parse(&git_stdout(&upstream_work, &["rev-parse", "HEAD"])).unwrap();
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let object_root = tmp.path().join("objects-v3");
        let config = AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 60,
            max_git_output_bytes: 16 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: object_root.clone(),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
                access_flush_interval_secs: 60,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            shutdown: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            async_materialize_concurrency: 1,
            use_gitoxide: true,
        };
        let state = Arc::new(ApiState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state.domain));
        let body = upload_pack_body(&[format!("want {commit} multi_ack thin-pack\n")]);
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".into()),
            all_upstream: std::collections::HashMap::from([("main".into(), commit.to_string())]),
        };
        let generation_task = direct_git_generation_task(
            &state,
            &materializer,
            &repo,
            &UpstreamAuth::Anonymous,
            &body,
            Some(&comparison),
            42,
        )
        .expect("advertised branch want should queue generation materialize");
        let warm_task = DirectGitWarmTask {
            imports: Arc::new(Semaphore::new(1)),
            materializer,
            repo,
            body,
            comparison: Some(comparison),
            request_id: 42,
            generation_task: Some(generation_task),
        };

        warm_task.spawn();

        let generation_root = object_root.join("repos/github.com/org/repo/generations");
        let generation_head =
            object_root.join("repos/github.com/org/repo/manifests/generation-head.json");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let has_generation_manifest = fs::read_dir(&generation_root)
                .ok()
                .into_iter()
                .flat_map(|entries| entries.filter_map(Result::ok))
                .any(|entry| entry.path().join("manifest.json").exists());
            if has_generation_manifest && generation_head.exists() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "proxy warm did not publish generation manifests asynchronously"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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
                access_flush_interval_secs: 60,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            shutdown: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            async_materialize_concurrency: git_cache_core::default_async_materialize_concurrency(),
            use_gitoxide: true,
        };
        let state = Arc::new(ApiState::try_new(config).unwrap());
        assert!(state.rate_limiter.check(), "first request consumes quota");

        let request = MaterializeRequest {
            repo: RepoKey::parse("evil.com/org/repo").unwrap(),
            selector: Selector::DefaultBranch,
            upstream_authorization: UpstreamAuthorizationMode::Required,
        };
        let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();

        let result =
            handle_checked_materialize_request(&state, MaterializeEndpoint::Resolve, request, auth)
                .await;

        match result {
            Err(error) => assert_eq!(error.status, StatusCode::TOO_MANY_REQUESTS),
            Ok(_) => panic!("rate-limited authenticated resolve should not succeed"),
        }
    }

    #[tokio::test]
    async fn healthz_fails_after_shutdown_begins() {
        let tmp = TempDir::new().unwrap();
        let config = AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            cache_root: tmp.path().join("cache"),
            upstream_root: None,
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 60,
            max_git_output_bytes: 16 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
                access_flush_interval_secs: 60,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            shutdown: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            async_materialize_concurrency: git_cache_core::default_async_materialize_concurrency(),
            use_gitoxide: true,
        };
        let state = Arc::new(ApiState::try_new(config).unwrap());
        let gate = ReadinessGate(Arc::clone(&state.shutting_down));

        let response = healthz(State(Arc::clone(&state))).await;
        assert_eq!(response.status(), StatusCode::OK);

        gate.begin_shutdown();

        let response = healthz(State(state)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Starts `run_until_shutdown` on an ephemeral port with a single `/slow`
    /// route that sleeps for `handler_delay` before responding. Returns the
    /// bound address, the shutdown flag readable by the test, a sender that
    /// triggers shutdown, and the server task handle.
    async fn spawn_drain_server(
        handler_delay: Duration,
        drain_timeout: Duration,
    ) -> (
        SocketAddr,
        Arc<AtomicBool>,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<CoreResult<()>>,
    ) {
        let shutting_down = Arc::new(AtomicBool::new(false));
        let gate = ReadinessGate(Arc::clone(&shutting_down));
        let app = Router::new().route(
            "/slow",
            get(move || async move {
                tokio::time::sleep(handler_delay).await;
                "done"
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(run_until_shutdown(
            listener,
            app,
            gate,
            Duration::ZERO,
            drain_timeout,
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        (addr, shutting_down, shutdown_tx, server)
    }

    #[tokio::test]
    async fn graceful_shutdown_lets_in_flight_request_finish_within_drain_timeout() {
        let (addr, shutting_down, shutdown_tx, server) =
            spawn_drain_server(Duration::from_millis(300), Duration::from_secs(5)).await;

        let request =
            tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });

        // Let the request reach the handler, then trigger shutdown mid-flight.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(()).unwrap();

        let response = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .expect("request should finish well before the drain timeout")
            .unwrap()
            .expect("in-flight request must not be killed by graceful shutdown");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "done");
        assert!(shutting_down.load(Ordering::SeqCst));

        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("server should stop promptly once the request drains")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn graceful_shutdown_kills_request_still_in_flight_after_drain_timeout() {
        let (addr, shutting_down, shutdown_tx, server) =
            spawn_drain_server(Duration::from_secs(60), Duration::from_millis(200)).await;

        let request =
            tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(()).unwrap();

        // The server must exit once the drain timeout elapses, without waiting
        // for the 60s handler.
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("server should force-exit at the drain deadline")
            .unwrap()
            .unwrap();
        assert!(shutting_down.load(Ordering::SeqCst));

        // In production the process exits at this point, cutting the request.
        // In-process we can only assert the server gave up on it: the request
        // is still in flight when run_until_shutdown returns.
        assert!(
            !request.is_finished(),
            "request should still be in flight when the server force-exits"
        );
        request.abort();
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
}
