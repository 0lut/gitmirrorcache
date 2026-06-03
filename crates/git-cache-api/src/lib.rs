use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures::Stream;
use git_cache_core::{
    AppConfig, GitCacheError, MaterializeRequest, Result as CoreResult, Selector,
};
use git_cache_domain::materializer::{advertise_refs, repo_from_git_path, upload_pack};
pub use git_cache_domain::AppState as DomainAppState;
use git_cache_domain::{
    frame_ref_advertisement, synthesize_ref_advertisement, AppState, Materializer,
    MaterializerExecutor,
};
use git_cache_worker::{InMemoryRepoLeaseManager, UpdateCoordinator, UpdateDisposition};
use http::{header, Method, StatusCode, Uri};
use serde::Serialize;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::AsyncRead;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::io::ReaderStream;

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
        .route("/v1/resolve", post(resolve))
        .route(
            "/git/session/{session_id}/{*repo_path}",
            any(git_session).layer(DefaultBodyLimit::max(git_body_limit)),
        );

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
    coordinator: UpdateCoordinator,
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
        let executor = Arc::new(MaterializerExecutor::new(Arc::clone(&domain)));
        let leases = Arc::new(InMemoryRepoLeaseManager::new());
        let coordinator = UpdateCoordinator::new(executor, leases);
        Materializer::new(Arc::clone(&domain)).enqueue_pending_generation_scan();
        Ok(Self {
            domain,
            coordinator,
            metrics: Arc::new(Metrics::default()),
            rate_limiter: Arc::new(rate_limiter),
        })
    }
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
         git_cache_git_upload_pack_total {}\n\
         git_cache_rate_limited_total {}\n\
         git_cache_git_remote_refs_total {}\n\
         git_cache_git_remote_upload_pack_total {}\n",
        state.metrics.materialize_total.load(Ordering::Relaxed),
        state
            .metrics
            .materialize_errors_total
            .load(Ordering::Relaxed),
        state.metrics.upload_pack_total.load(Ordering::Relaxed),
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
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    handle_materialize_request(&state, request).await
}

async fn resolve(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    handle_materialize_request(&state, request).await
}

async fn handle_materialize_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
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

    let use_coordinator = matches!(
        request.selector,
        Selector::Branch(_) | Selector::DefaultBranch
    );

    let verified_by_coordinator = if use_coordinator {
        let outcome = state
            .coordinator
            .read_through(request.repo.clone(), request.selector.clone())
            .await;
        match outcome {
            Ok(o) if o.disposition == UpdateDisposition::LeaseBusy => {
                return Err(ApiError {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    message: "update in progress, retry later".into(),
                });
            }
            Err(error) => {
                state
                    .metrics
                    .materialize_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(error.into());
            }
            Ok(_) => {}
        }
        true
    } else {
        false
    };

    let materializer = Materializer::new(Arc::clone(&state.domain));
    let result = if verified_by_coordinator {
        materializer
            .materialize_after_upstream_validation(request)
            .await
    } else {
        materializer.materialize(request).await
    };

    match result {
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

async fn git_session(
    State(state): State<Arc<ApiState>>,
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

    let session_id = match git_cache_core::SessionId::parse(&session_id) {
        Ok(id) => id,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let repo = match repo_from_git_path(&repo_path) {
        Ok(repo) => repo,
        Err(error) => return ApiError::from(error).into_response(),
    };

    let materializer = Materializer::new(Arc::clone(&state.domain));
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
        advertise_refs(&state.domain, &session_repo)
            .await
            .map(|output| {
                git_response(
                    "application/x-git-upload-pack-advertisement",
                    frame_ref_advertisement(&output),
                )
            })
    } else if method == Method::POST && path.ends_with("/git-upload-pack") {
        state
            .metrics
            .upload_pack_total
            .fetch_add(1, Ordering::Relaxed);
        upload_pack(&state.domain, &session_repo, body)
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

/// Direct Git remote handler: `/git/{host}/{owner}/{repo}.git/...`
///
/// This is the read-through handler that makes the cache behave like a normal
/// Git remote. No prior `/v1/materialize` call is needed.
async fn git_repo(
    State(state): State<Arc<ApiState>>,
    Path(repo_path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
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
        state
            .metrics
            .git_remote_refs_total
            .fetch_add(1, Ordering::Relaxed);

        // Fetch upstream refs via ls-remote and synthesize the pkt-line
        // response directly.  No objects are fetched — the repo may not
        // even exist locally yet.  Objects are fetched lazily when the
        // client actually issues an upload-pack POST.
        let comparison = match materializer.upstream_refs(&repo).await {
            Ok(c) => c,
            Err(error) => return ApiError::from(error).into_response(),
        };

        let output = synthesize_ref_advertisement(&comparison);
        git_response(
            "application/x-git-upload-pack-advertisement",
            frame_ref_advertisement(&output),
        )
    } else if method == Method::POST && path.ends_with("/git-upload-pack") {
        state
            .metrics
            .git_remote_upload_pack_total
            .fetch_add(1, Ordering::Relaxed);

        match materializer.handle_upload_pack(&repo, &body).await {
            Ok(mut process) => {
                let permit = process.take_permit();
                let child = process.child;
                let reader_stream = ReaderStream::new(process.stdout);
                let max_bytes = state.domain.config.max_git_output_bytes as u64;
                let guarded = ChildGuardStream {
                    inner: reader_stream,
                    _child: child,
                    bytes_sent: 0,
                    max_bytes,
                    _permit: permit,
                };
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/x-git-upload-pack-result")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .body(Body::from_stream(guarded))
                    .expect("git upload-pack response")
            }
            Err(error) => ApiError::from(error).into_response(),
        }
    } else {
        ApiError::from(GitCacheError::Unsupported(format!(
            "unsupported git request: {method} {path}"
        )))
        .into_response()
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

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    checked_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Default)]
struct Metrics {
    materialize_total: AtomicU64,
    materialize_errors_total: AtomicU64,
    upload_pack_total: AtomicU64,
    rate_limited_total: AtomicU64,
    git_remote_refs_total: AtomicU64,
    git_remote_upload_pack_total: AtomicU64,
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

pub fn empty_body() -> Body {
    Body::empty()
}

/// Wraps a `ReaderStream` and holds a child process handle to keep the process
/// alive for the duration of the HTTP response body stream. Also holds the
/// semaphore permit so it is not released until the stream is fully consumed.
struct ChildGuardStream<R: AsyncRead + Unpin> {
    inner: ReaderStream<R>,
    _child: tokio::process::Child,
    bytes_sent: u64,
    max_bytes: u64,
    _permit: Option<OwnedSemaphorePermit>,
}

impl<R: AsyncRead + Unpin> Stream for ChildGuardStream<R> {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
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
    use git_cache_core::ObjectStoreConfig;
    use git_cache_domain::parse_want_lines;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn rate_limiter_blocks_after_limit() {
        let limiter = RateLimiter::new(2);
        assert!(limiter.check());
        assert!(limiter.check());
        assert!(!limiter.check());
    }

    #[tokio::test]
    async fn receive_pack_requests_are_rejected_before_session_lookup() {
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
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
            max_concurrent_generation_verifications: 1,
        };
        let api_state = ApiState::try_new(config).unwrap();
        let mut query = HashMap::new();
        query.insert("service".to_string(), "git-receive-pack".to_string());

        let response = git_session(
            State(Arc::new(api_state)),
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
