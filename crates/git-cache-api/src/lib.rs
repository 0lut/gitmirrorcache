use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine;
use futures::Stream;
use git_cache_core::{
    AppConfig, BranchName, GitCacheError, MaterializeRequest, RepoKey, Result as CoreResult,
    Selector, UpstreamAuth, UpstreamAuthorizationMode,
};
use git_cache_domain::materializer::{advertise_refs, repo_from_git_path};
pub use git_cache_domain::AppState as DomainAppState;
use git_cache_domain::{
    frame_ref_advertisement, synthesize_protected_ref_advertisement, synthesize_ref_advertisement,
    AppState, Materializer, MaterializerExecutor,
};
use git_cache_git::UploadPackProcess;
use git_cache_worker::{InMemoryRepoLeaseManager, UpdateCoordinator, UpdateDisposition};
use http::{header, HeaderMap, Method, StatusCode, Uri};
use serde::{Deserialize, Serialize};
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

const GIT_UPLOAD_PACK_STREAM_BUFFER_BYTES: usize = 64 * 1024;

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
    repo_authorizer: RepoAuthorizer,
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
        Ok(Self {
            domain,
            coordinator,
            repo_authorizer: RepoAuthorizer::new(),
            metrics: Arc::new(Metrics::default()),
            rate_limiter: Arc::new(rate_limiter),
        })
    }
}

#[derive(Clone)]
struct RepoAuthorizer {
    client: reqwest::Client,
    github_api_base_url: String,
    git_base_url_overrides: HashMap<String, String>,
}

impl RepoAuthorizer {
    fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            github_api_base_url: "https://api.github.com".to_string(),
            git_base_url_overrides: HashMap::new(),
        }
    }

    #[cfg(test)]
    fn with_mock_base_url(base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            github_api_base_url: base_url.clone(),
            git_base_url_overrides: [
                ("github.com".to_string(), base_url.clone()),
                ("gitlab.com".to_string(), base_url.clone()),
                ("bitbucket.org".to_string(), base_url),
            ]
            .into_iter()
            .collect(),
        }
    }

    async fn authorize(
        &self,
        domain: &Arc<AppState>,
        repo: &RepoKey,
        auth: &UpstreamAuth,
    ) -> CoreResult<RepoAuthorization> {
        let materializer = Materializer::new(Arc::clone(domain)).using_upstream_auth(auth);
        materializer.validate_host(repo)?;
        let provider = RepoProvider::from_repo(repo);

        if domain.config.upstream_root.is_none() {
            if self.public_git_probe(repo, provider).await? {
                return Ok(RepoAuthorization::public());
            }

            if auth.is_authenticated() {
                // Public reachability failed, so the request token is now the
                // repo-access fallback. Once this succeeds, domain code may
                // treat the request as repo-authorized and skip duplicate
                // per-object upstream proof on hot upload-pack POSTs.
                match provider {
                    RepoProvider::GitHub => self.authorize_github(repo, auth).await?,
                    RepoProvider::GitLab | RepoProvider::Bitbucket | RepoProvider::Generic => {
                        self.authorize_with_git_probe(domain, repo, auth).await?;
                    }
                }
                return Ok(RepoAuthorization::upstream(auth.clone()));
            }

            return Err(GitCacheError::Unauthorized(
                "repository is not publicly reachable".into(),
            ));
        }

        self.authorize_with_git_probe(domain, repo, auth).await?;
        Ok(RepoAuthorization::upstream(auth.clone()))
    }

    async fn effective_auth_for_ref_advertisement(
        &self,
        domain: &Arc<AppState>,
        repo: &RepoKey,
        auth: &UpstreamAuth,
    ) -> CoreResult<UpstreamAuth> {
        let materializer = Materializer::new(Arc::clone(domain)).using_upstream_auth(auth);
        materializer.validate_host(repo)?;

        if auth.is_authenticated()
            && domain.config.upstream_root.is_none()
            && self
                .public_git_probe(repo, RepoProvider::from_repo(repo))
                .await?
        {
            return Ok(UpstreamAuth::Anonymous);
        }
        Ok(auth.clone())
    }

    async fn authorize_with_git_probe(
        &self,
        domain: &Arc<AppState>,
        repo: &RepoKey,
        auth: &UpstreamAuth,
    ) -> CoreResult<()> {
        let materializer = Materializer::new(Arc::clone(domain)).using_upstream_auth(auth);
        let remote = materializer.upstream_url(repo)?;
        domain
            .git
            .with_upstream_auth(&remote, auth)?
            .ls_remote_default_branch(&remote)
            .await?;
        Ok(())
    }

    async fn public_git_probe(&self, repo: &RepoKey, provider: RepoProvider) -> CoreResult<bool> {
        let url = format!(
            "{}/{}/{}.git/info/refs?service=git-upload-pack",
            self.git_base_url(repo).trim_end_matches('/'),
            repo.owner(),
            repo.name()
        );
        let response = self
            .client
            .get(url)
            .header("Git-Protocol", "version=2")
            .header(header::USER_AGENT, "git-cache-api")
            .send()
            .await
            .map_err(|err| {
                GitCacheError::UpstreamUnavailable(format!(
                    "{} public git reachability check failed: {err}",
                    provider.label()
                ))
            })?;

        let status = response.status();
        if status.is_success() {
            return Ok(true);
        }
        if matches!(status.as_u16(), 401 | 403 | 404) {
            return Ok(false);
        }
        if status.as_u16() == 429 || status.is_server_error() {
            return Err(GitCacheError::UpstreamUnavailable(format!(
                "{} public git reachability check returned HTTP {}",
                provider.label(),
                status.as_u16()
            )));
        }
        Ok(false)
    }

    fn git_base_url(&self, repo: &RepoKey) -> String {
        self.git_base_url_overrides
            .get(repo.host())
            .cloned()
            .unwrap_or_else(|| format!("https://{}", repo.host()))
    }

    async fn authorize_github(&self, repo: &RepoKey, auth: &UpstreamAuth) -> CoreResult<()> {
        let token = basic_password_for_github_rest(auth)?;
        let repo_url = format!(
            "{}/repos/{}/{}",
            self.github_api_base_url.trim_end_matches('/'),
            repo.owner(),
            repo.name()
        );
        let repo_response: GithubRepoResponse = self
            .github_get_json(&repo_url, &token, "repo metadata")
            .await?;
        let default_branch = BranchName::parse(repo_response.default_branch)?;
        let ref_url = format!(
            "{}/repos/{}/{}/git/ref/heads/{}",
            self.github_api_base_url.trim_end_matches('/'),
            repo.owner(),
            repo.name(),
            default_branch.as_str()
        );
        let _: GithubRefResponse = self
            .github_get_json(&ref_url, &token, "default branch ref")
            .await?;
        Ok(())
    }

    async fn github_get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
        token: &str,
        endpoint: &'static str,
    ) -> CoreResult<T> {
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(header::USER_AGENT, "git-cache-api")
            .send()
            .await
            .map_err(|err| {
                GitCacheError::UpstreamUnavailable(format!(
                    "github {endpoint} access check failed: {err}"
                ))
            })?;

        let status = response.status();
        if status.is_success() {
            return response.json::<T>().await.map_err(|err| {
                GitCacheError::UpstreamUnavailable(format!(
                    "github {endpoint} response was invalid: {err}"
                ))
            });
        }

        let rate_limited = status.as_u16() == 429
            || (status.as_u16() == 403
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .and_then(|value| value.to_str().ok())
                    == Some("0"));
        if rate_limited {
            return Err(GitCacheError::UpstreamUnavailable(format!(
                "github {endpoint} access check was rate limited"
            )));
        }

        match status.as_u16() {
            401 => Err(GitCacheError::Unauthorized(
                "upstream token was rejected by GitHub".into(),
            )),
            403 => Err(GitCacheError::Forbidden(format!(
                "upstream token cannot read repository {endpoint}"
            ))),
            404 => Err(GitCacheError::Unauthorized(
                "upstream token cannot access repository".into(),
            )),
            code => Err(GitCacheError::UpstreamUnavailable(format!(
                "github {endpoint} access check returned HTTP {code}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
struct RepoAuthorization {
    effective_auth: UpstreamAuth,
}

impl RepoAuthorization {
    fn public() -> Self {
        Self {
            effective_auth: UpstreamAuth::Anonymous,
        }
    }

    fn upstream(auth: UpstreamAuth) -> Self {
        Self {
            effective_auth: auth,
        }
    }

    fn into_effective_auth(self) -> UpstreamAuth {
        self.effective_auth
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoProvider {
    GitHub,
    GitLab,
    Bitbucket,
    Generic,
}

impl RepoProvider {
    fn from_repo(repo: &RepoKey) -> Self {
        match repo.host() {
            "github.com" => Self::GitHub,
            "gitlab.com" => Self::GitLab,
            "bitbucket.org" => Self::Bitbucket,
            _ => Self::Generic,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Bitbucket => "bitbucket",
            Self::Generic => "git",
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubRepoResponse {
    default_branch: String,
}

#[derive(Debug, Deserialize)]
struct GithubRefResponse {
    #[allow(dead_code)]
    object: GithubRefObject,
}

#[derive(Debug, Deserialize)]
struct GithubRefObject {
    #[allow(dead_code)]
    sha: String,
}

fn basic_password_for_github_rest(auth: &UpstreamAuth) -> CoreResult<String> {
    let Some(raw_header) = auth.raw_header() else {
        return Err(GitCacheError::Unauthorized(
            "upstream authorization is required".into(),
        ));
    };
    let Some(encoded) = raw_header.trim().strip_prefix("Basic ") else {
        return Err(GitCacheError::Validation(
            "upstream authorization must use Basic authentication".into(),
        ));
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|_| GitCacheError::Validation("invalid Basic authorization encoding".into()))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|_| GitCacheError::Validation("invalid Basic authorization credentials".into()))?;
    let Some((_, password)) = decoded.split_once(':') else {
        return Err(GitCacheError::Validation(
            "Basic authorization must include a token password".into(),
        ));
    };
    if password.trim().is_empty() {
        return Err(GitCacheError::Validation(
            "Basic authorization token is empty".into(),
        ));
    }
    Ok(password.to_string())
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

    let AuthorizedMaterializeRequest { request, auth } =
        match authorize_materialize_repo(state, request, auth).await {
            Ok(authorized) => authorized,
            Err(error) => {
                state
                    .metrics
                    .materialize_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                return Err(error);
            }
        };

    materialize_authorized_request(state, request, auth).await
}

async fn materialize_authorized_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    let use_coordinator = !request.uses_upstream_auth(&auth)
        && matches!(
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

    let materializer = Materializer::new(Arc::clone(&state.domain)).using_upstream_auth(&auth);
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

async fn handle_resolve_request(
    state: &Arc<ApiState>,
    request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<Response, ApiError> {
    if request.uses_upstream_auth(&auth) {
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

        let AuthorizedMaterializeRequest { request, auth } =
            match authorize_materialize_repo(state, request, auth).await {
                Ok(authorized) => authorized,
                Err(error) => {
                    state
                        .metrics
                        .materialize_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(error);
                }
            };

        if !request.uses_upstream_auth(&auth) {
            return materialize_authorized_request(state, request, auth).await;
        }

        let materializer = Materializer::new(Arc::clone(&state.domain)).using_upstream_auth(&auth);
        return match materializer.resolve(request).await {
            Ok(response) => Ok(Json(response).into_response()),
            Err(error) => {
                state
                    .metrics
                    .materialize_errors_total
                    .fetch_add(1, Ordering::Relaxed);
                Err(error.into())
            }
        };
    }

    handle_materialize_request(state, request, auth).await
}

struct AuthorizedMaterializeRequest {
    request: MaterializeRequest,
    auth: UpstreamAuth,
}

async fn authorize_materialize_repo(
    state: &Arc<ApiState>,
    mut request: MaterializeRequest,
    auth: UpstreamAuth,
) -> Result<AuthorizedMaterializeRequest, ApiError> {
    if request.requires_upstream_auth() && !auth.is_authenticated() {
        return Err(
            GitCacheError::Unauthorized("upstream authorization is required".into()).into(),
        );
    }
    let auth = state
        .repo_authorizer
        .authorize(&state.domain, &request.repo, &auth)
        .await
        .map(RepoAuthorization::into_effective_auth)
        .map_err(ApiError::from)?;
    if !auth.is_authenticated() {
        request.upstream_authorization = UpstreamAuthorizationMode::Anonymous;
    }
    Ok(AuthorizedMaterializeRequest { request, auth })
}

async fn git_session(
    State(state): State<Arc<ApiState>>,
    Path((session_id, repo_path)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
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
    let session_token = match session_bearer_token(&headers) {
        Ok(token) => token,
        Err(error) => return error.into_response(),
    };
    let session_repo = match materializer
        .session_repo_from_manifest(&repo, session_id, session_token.as_deref())
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
        state
            .domain
            .git
            .upload_pack_spawn(&session_repo, body)
            .await
            .map(|process| stream_upload_pack_response(&state, process))
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
    headers: HeaderMap,
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
        let auth = match direct_git_upstream_auth(&headers) {
            Ok(auth) => auth,
            Err(error) => return error.into_response(),
        };
        state
            .metrics
            .git_remote_refs_total
            .fetch_add(1, Ordering::Relaxed);
        let auth = match state
            .repo_authorizer
            .effective_auth_for_ref_advertisement(&state.domain, &repo, &auth)
            .await
        {
            Ok(auth) => auth,
            Err(error) => return ApiError::from(error).into_response(),
        };
        let materializer = materializer.using_upstream_auth(&auth);

        // Fetch upstream refs via ls-remote and synthesize the pkt-line
        // response directly. No objects are fetched here. Anonymous POSTs can
        // use refs published by prior public materialization/fetches to avoid
        // a second upstream comparison; misses still fall back to request
        // scoped upstream proof in handle_upload_pack.
        let comparison = match materializer.upstream_refs(&repo).await {
            Ok(c) => c,
            Err(error) => return ApiError::from(error).into_response(),
        };

        let output = if auth.is_authenticated() {
            synthesize_protected_ref_advertisement(&comparison)
        } else {
            synthesize_ref_advertisement(&comparison)
        };
        git_response(
            "application/x-git-upload-pack-advertisement",
            frame_ref_advertisement(&output),
        )
    } else if method == Method::POST && path.ends_with("/git-upload-pack") {
        let auth = match direct_git_upstream_auth(&headers) {
            Ok(auth) => auth,
            Err(error) => return error.into_response(),
        };
        state
            .metrics
            .git_remote_upload_pack_total
            .fetch_add(1, Ordering::Relaxed);
        // Direct Git POST is stateless, so it must establish repo access here
        // instead of relying on the prior info/refs request. The fast public
        // gate is provider-neutral Smart HTTP; private GitHub requests use
        // REST, and other private providers fall back to authenticated Git
        // proof until provider-specific adapters exist.
        let auth = match state
            .repo_authorizer
            .authorize(&state.domain, &repo, &auth)
            .await
        {
            Ok(authorization) => authorization.into_effective_auth(),
            Err(error) => return ApiError::from(error).into_response(),
        };
        let materializer = materializer.using_upstream_auth(&auth);

        match materializer.handle_upload_pack(&repo, &body).await {
            Ok(process) => stream_upload_pack_response(&state, process),
            Err(error) => ApiError::from(error).into_response(),
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

fn session_bearer_token(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ApiError {
        status: StatusCode::UNAUTHORIZED,
        message: "session authorization header must be valid ASCII".into(),
    })?;
    let mut parts = value.splitn(2, char::is_whitespace);
    let scheme = parts.next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Ok(None);
    }
    let Some(token) = parts.next() else {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "session bearer token is invalid".into(),
        });
    };
    if token.trim().is_empty()
        || token
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "session bearer token is invalid".into(),
        });
    }
    Ok(Some(token.trim().to_string()))
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
        MaterializeRequest, ObjectStoreConfig, RepoKey, UpstreamAuthorizationMode,
    };
    use git_cache_domain::parse_want_lines;
    use std::net::SocketAddr;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Mutex as StdMutex;
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
    fn upstream_api_auth_ignores_gateway_bearer_authorization() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer gateway-token".parse().unwrap(),
        );

        let auth = upstream_api_auth(&headers).unwrap();

        assert_eq!(auth, UpstreamAuth::Anonymous);
    }

    #[test]
    fn session_bearer_token_accepts_case_insensitive_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "bearer gcs_token".parse().unwrap());

        let token = session_bearer_token(&headers).unwrap();

        assert_eq!(token.as_deref(), Some("gcs_token"));
    }

    #[test]
    fn session_bearer_token_ignores_unrelated_authorization_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());

        let token = session_bearer_token(&headers).unwrap();

        assert_eq!(token, None);
    }

    #[test]
    fn basic_password_for_github_rest_decodes_token_password() {
        let auth =
            UpstreamAuth::parse_header(&basic_header("x-access-token", "ghp_secret")).unwrap();

        let token = basic_password_for_github_rest(&auth).unwrap();

        assert_eq!(token, "ghp_secret");
    }

    #[test]
    fn basic_password_for_github_rest_rejects_empty_token_password() {
        let auth = UpstreamAuth::parse_header(&basic_header("x-access-token", "")).unwrap();

        let error = basic_password_for_github_rest(&auth).unwrap_err();

        assert!(matches!(error, GitCacheError::Validation(_)));
    }

    #[tokio::test]
    async fn authenticated_public_github_authorizer_ignores_token_without_rest() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["github.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let auth =
            UpstreamAuth::parse_header(&basic_header("x-access-token", "ghp_secret")).unwrap();

        let authorization = authorizer.authorize(&domain, &repo, &auth).await.unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), UpstreamAuth::Anonymous);
        let hits = hits.lock().unwrap().clone();
        assert_eq!(
            hits,
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
    }

    #[tokio::test]
    async fn authenticated_private_github_authorizer_uses_rest_fallback() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) =
            start_provider_authorizer_mock(StatusCode::UNAUTHORIZED).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["github.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let auth =
            UpstreamAuth::parse_header(&basic_header("x-access-token", "ghp_secret")).unwrap();

        let authorization = authorizer.authorize(&domain, &repo, &auth).await.unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), auth);
        let hits = hits.lock().unwrap().clone();
        assert_eq!(
            hits,
            vec![
                "/org/repo.git/info/refs auth= git-protocol=version=2".to_string(),
                "/repos/org/repo auth=Bearer ghp_secret git-protocol=".to_string(),
                "/repos/org/repo/git/ref/heads/main auth=Bearer ghp_secret git-protocol="
                    .to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn anonymous_github_authorizer_uses_public_probe_not_git_or_rest() {
        let tmp = TempDir::new().unwrap();
        let (fake_git, git_log) = fake_git_binary(&tmp);
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                fake_git,
                vec!["github.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("github.com/org/repo").unwrap();

        let authorization = authorizer
            .authorize(&domain, &repo, &UpstreamAuth::Anonymous)
            .await
            .unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), UpstreamAuth::Anonymous);
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
        assert!(
            !git_log.exists(),
            "public GitHub probe should not spawn git"
        );
    }

    #[tokio::test]
    async fn materialize_authorization_downgrades_public_github_auth_to_public_request() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["github.com".into()],
            ))
            .unwrap(),
        );
        let mut state = ApiState::with_domain(RateLimiter::new(1), domain).unwrap();
        state.repo_authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let state = Arc::new(state);
        let request = MaterializeRequest {
            repo: RepoKey::parse("github.com/org/repo").unwrap(),
            selector: Selector::DefaultBranch,
            mode: Default::default(),
            upstream_authorization: UpstreamAuthorizationMode::Required,
        };
        let auth =
            UpstreamAuth::parse_header(&basic_header("x-access-token", "ghp_secret")).unwrap();

        let authorized = authorize_materialize_repo(&state, request, auth)
            .await
            .unwrap();
        server.abort();

        assert_eq!(authorized.auth, UpstreamAuth::Anonymous);
        assert_eq!(
            authorized.request.upstream_authorization,
            UpstreamAuthorizationMode::Anonymous
        );
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
    }

    #[tokio::test]
    async fn public_github_ref_advertisement_downgrades_auth_without_rest() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["github.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let auth =
            UpstreamAuth::parse_header(&basic_header("x-access-token", "ghp_secret")).unwrap();

        let effective_auth = authorizer
            .effective_auth_for_ref_advertisement(&domain, &repo, &auth)
            .await
            .unwrap();
        server.abort();

        assert_eq!(effective_auth, UpstreamAuth::Anonymous);
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
    }

    #[tokio::test]
    async fn authenticated_public_gitlab_authorizer_ignores_token_without_rest() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["gitlab.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("gitlab.com/org/repo").unwrap();
        let auth = UpstreamAuth::parse_header(&basic_header("user", "token")).unwrap();

        let authorization = authorizer.authorize(&domain, &repo, &auth).await.unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), UpstreamAuth::Anonymous);
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
    }

    #[tokio::test]
    async fn authenticated_private_gitlab_authorizer_uses_git_probe_fallback() {
        let tmp = TempDir::new().unwrap();
        let (fake_git, git_log) = fake_git_binary(&tmp);
        let (base_url, hits, server) =
            start_provider_authorizer_mock(StatusCode::UNAUTHORIZED).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                fake_git,
                vec!["gitlab.com".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("gitlab.com/org/repo").unwrap();
        let auth = UpstreamAuth::parse_header(&basic_header("user", "token")).unwrap();

        let authorization = authorizer.authorize(&domain, &repo, &auth).await.unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), auth);
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
        let git_args = std::fs::read_to_string(git_log).unwrap();
        assert!(
            git_args.contains("ls-remote --symref -- https://gitlab.com/org/repo.git HEAD"),
            "unexpected fake git args: {git_args}"
        );
    }

    #[tokio::test]
    async fn authenticated_public_bitbucket_authorizer_ignores_token_without_rest() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits, server) = start_provider_authorizer_mock(StatusCode::OK).await;
        let domain = Arc::new(
            AppState::try_new(api_test_config(
                &tmp,
                None,
                PathBuf::from("git"),
                vec!["bitbucket.org".into()],
            ))
            .unwrap(),
        );
        let authorizer = RepoAuthorizer::with_mock_base_url(base_url);
        let repo = RepoKey::parse("bitbucket.org/org/repo").unwrap();
        let auth = UpstreamAuth::parse_header(&basic_header("user", "token")).unwrap();

        let authorization = authorizer.authorize(&domain, &repo, &auth).await.unwrap();
        server.abort();

        assert_eq!(authorization.into_effective_auth(), UpstreamAuth::Anonymous);
        assert_eq!(
            hits.lock().unwrap().clone(),
            vec!["/org/repo.git/info/refs auth= git-protocol=version=2".to_string()]
        );
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
            session_ttl_seconds: 3600,
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
            session_cleanup_interval_secs: 300,
            max_concurrent_generation_verifications: 1,
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
            HeaderMap::new(),
            Method::GET,
            Uri::from_static(
                "/git/session/not-a-session/github.com/org/repo.git/info/refs?service=git-receive-pack",
            ),
            Bytes::new(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    fn api_test_config(
        tmp: &TempDir,
        upstream_root: Option<PathBuf>,
        git_binary: PathBuf,
        allowed_upstream_hosts: Vec<String>,
    ) -> AppConfig {
        AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            public_base_url: "http://127.0.0.1:0".into(),
            cache_root: tmp.path().join("cache"),
            upstream_root,
            git_binary,
            git_timeout_seconds: 60,
            max_git_output_bytes: 16 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            session_ttl_seconds: 3600,
            upstream_auth_token_env: None,
            rate_limit_per_minute: 1,
            allowed_upstream_hosts,
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
            max_concurrent_generation_verifications: 1,
        }
    }

    fn basic_header(username: &str, password: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
        )
    }

    fn fake_git_binary(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let script = tmp.path().join("fake-git.sh");
        let log = tmp.path().join("fake-git.log");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nprintf 'ref: refs/heads/main\\tHEAD\\n'\nprintf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\tHEAD\\n'\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            let mut permissions = std::fs::metadata(&script).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&script, permissions).unwrap();
        }
        (script, log)
    }

    async fn start_provider_authorizer_mock(
        public_probe_status: StatusCode,
    ) -> (
        String,
        Arc<StdMutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let hits = Arc::new(StdMutex::new(Vec::new()));
        let routes = Router::new().fallback({
            let hits = Arc::clone(&hits);
            move |headers: HeaderMap, uri: Uri| {
                let hits = Arc::clone(&hits);
                async move {
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    let git_protocol = headers
                        .get("Git-Protocol")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    hits.lock().unwrap().push(format!(
                        "{} auth={} git-protocol={}",
                        uri.path(),
                        auth,
                        git_protocol
                    ));
                    if uri.path() == "/org/repo.git/info/refs" {
                        return if public_probe_status == StatusCode::OK {
                            Body::from("001e# service=git-upload-pack\n0000").into_response()
                        } else {
                            public_probe_status.into_response()
                        };
                    }
                    if auth != "Bearer ghp_secret" {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    match uri.path() {
                        "/repos/org/repo" => Json(serde_json::json!({
                            "default_branch": "main",
                        }))
                        .into_response(),
                        "/repos/org/repo/git/ref/heads/main" => Json(serde_json::json!({
                            "object": {
                                "sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            },
                        }))
                        .into_response(),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        (format!("http://{addr}"), hits, server)
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
