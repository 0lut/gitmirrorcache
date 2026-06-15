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
    let error = decode_git_request_body(&headers, Bytes::from_static(b"data"), 1024).unwrap_err();
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

    let commit = CommitSha::parse(git_stdout(&upstream_work, &["rev-parse", "HEAD"])).unwrap();
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
    let shallow_body = upload_pack_body(&[
        format!("want {commit} multi_ack thin-pack\n"),
        "deepen 10\n".to_string(),
    ]);
    let body = upload_pack_body(&[format!("want {commit} multi_ack thin-pack\n")]);
    let comparison = UpstreamRefComparison {
        default_branch: Some("main".into()),
        all_upstream: std::collections::HashMap::from([("main".into(), commit.to_string())]),
    };
    assert!(
        direct_git_generation_task(
            &state,
            &materializer,
            &repo,
            &UpstreamAuth::Anonymous,
            &shallow_body,
            Some(&comparison),
            42,
        )
        .is_none(),
        "shallow/deepen proxy requests should not queue full-history materialization"
    );
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
    assert_eq!(generation_task.requests.len(), 1);
    assert_eq!(
        generation_task.requests[0].0.selector,
        Selector::DefaultBranch
    );
    assert_eq!(generation_task.requests[0].1, commit);
    let warm_task = DirectGitWarmTask {
        imports: Arc::new(Semaphore::new(1)),
        materializer,
        repo,
        body,
        comparison: Some(comparison),
        request_id: 42,
        generation_task: Some(generation_task),
    };

    warm_task.spawn().await.unwrap();

    let generation_root = object_root.join("repos/github.com/org/repo/generations");
    let generation_head =
        object_root.join("repos/github.com/org/repo/manifests/generation-head.json");
    let has_generation_manifest = fs::read_dir(&generation_root)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .any(|entry| entry.path().join("manifest.json").exists());
    assert!(
        has_generation_manifest,
        "proxy warm did not publish a generation manifest"
    );
    assert!(
        generation_head.exists(),
        "proxy warm did not publish the generation head manifest"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_warm_task_publishes_served_commit_when_branch_moves() {
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

    let served_commit =
        CommitSha::parse(git_stdout(&upstream_work, &["rev-parse", "HEAD"])).unwrap();
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
    let body = upload_pack_body(&[format!("want {served_commit} multi_ack thin-pack\n")]);
    let comparison = UpstreamRefComparison {
        default_branch: Some("main".into()),
        all_upstream: std::collections::HashMap::from([("main".into(), served_commit.to_string())]),
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

    // Branch moves upstream after the client was served `served_commit`.
    fs::write(upstream_work.join("README.md"), "moved\n").unwrap();
    run_git(&upstream_work, &["add", "README.md"]);
    run_git(&upstream_work, &["commit", "-m", "moved"]);
    run_git(&upstream_work, &["push", "origin", "main"]);
    let moved_commit =
        CommitSha::parse(git_stdout(&upstream_work, &["rev-parse", "HEAD"])).unwrap();
    assert_ne!(served_commit, moved_commit);

    let warm_task = DirectGitWarmTask {
        imports: Arc::new(Semaphore::new(1)),
        materializer: materializer.clone(),
        repo: repo.clone(),
        body,
        comparison: Some(comparison),
        request_id: 42,
        generation_task: Some(generation_task),
    };

    warm_task.spawn().await.unwrap();

    let manifest = materializer
        .get_commit_manifest(&repo, &served_commit)
        .await
        .unwrap();
    assert!(
        manifest.is_some_and(|manifest| manifest.complete),
        "served commit must have a complete commit manifest even after the branch moved"
    );
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

    let request = tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });

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

    let request = tokio::spawn(async move { reqwest::get(format!("http://{addr}/slow")).await });

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
