use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use git_cache_core::{
    AppConfig, GitCacheError, MaterializeRequest, RequestMode, Result as CoreResult, Selector,
};
use git_cache_domain::materializer::{advertise_refs, repo_from_git_path, upload_pack};
use git_cache_domain::{AppState, Materializer, MaterializerExecutor};
use git_cache_worker::{InMemoryRepoLeaseManager, UpdateCoordinator, UpdateDisposition};
use http::{header, Method, StatusCode, Uri};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use git_cache_domain::AppState as DomainAppState;

pub fn app(config: AppConfig) -> Router {
    app_result(config).expect("failed to initialize git-cache-api")
}

pub fn app_result(config: AppConfig) -> CoreResult<Router> {
    let state = Arc::new(ApiState::try_new(config)?);

    Ok(Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/v1/materialize", post(materialize))
        .route("/v1/resolve", post(resolve))
        .route("/git/session/{session_id}/{*repo_path}", any(git_session))
        .with_state(state))
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
        let executor = Arc::new(MaterializerExecutor::new(Arc::clone(&domain)));
        let leases = Arc::new(InMemoryRepoLeaseManager::new());
        let coordinator = UpdateCoordinator::new(executor, leases);
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
    State(state): State<Arc<ApiState>>,
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

    let mut request = request;

    let use_coordinator = matches!(
        request.selector,
        Selector::Branch(_) | Selector::DefaultBranch
    );

    if use_coordinator {
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
            Ok(_) => {
                // Coordinator already did fetch+publish; use Cached mode so
                // materialize just creates a session from the fresh local data
                // without hitting upstream again.
                request.mode = RequestMode::Cached;
            }
        }
    }

    let materializer = Materializer::new(Arc::clone(&state.domain));
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
    State(state): State<Arc<ApiState>>,
    Json(request): Json<MaterializeRequest>,
) -> Result<Response, ApiError> {
    let materializer = Materializer::new(Arc::clone(&state.domain));
    let response = materializer.materialize(request).await?;
    Ok(Json(response).into_response())
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
        advertise_refs(&state.domain, &session_repo).await.map(|output| {
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
    use git_cache_core::ObjectStoreConfig;
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
            cached_ref_max_staleness_seconds: 300,
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
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
}
