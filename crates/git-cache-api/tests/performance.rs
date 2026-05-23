//! Performance tests for the API server.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tempfile::TempDir;
use tokio::net::TcpListener;

struct TestServer {
    addr: SocketAddr,
    tmp: TempDir,
    _upstream_work: PathBuf,
    _upstream_bare: PathBuf,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_rate_limit(0).await
    }

    async fn start_with_rate_limit(rate_limit: u32) -> Self {
        let tmp = TempDir::new().unwrap();
        let upstream_bare = tmp.path().join("upstreams/github.com/org/repo.git");
        let upstream_work = tmp.path().join("work");

        std::fs::create_dir_all(upstream_bare.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&upstream_work).unwrap();

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
        std::fs::write(upstream_work.join("README.md"), "initial\n").unwrap();
        run_git(&upstream_work, &["add", "README.md"]);
        run_git(&upstream_work, &["commit", "-m", "initial"]);
        run_git(&upstream_work, &["branch", "-M", "main"]);
        run_git(
            &upstream_work,
            &["remote", "add", "origin", upstream_bare.to_str().unwrap()],
        );
        run_git(&upstream_work, &["push", "origin", "main"]);
        run_git(&upstream_bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let config = AppConfig {
            bind_addr: addr,
            public_base_url: format!("http://{addr}"),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 120,
            max_git_output_bytes: 64 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            session_ttl_seconds: 3600,
            upstream_auth_token_env: None,
            rate_limit_per_minute: rate_limit,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: GitRemoteConfig {
                enabled: true,
                ..Default::default()
            },
            compaction: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
        };

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        Self {
            addr,
            tmp,
            _upstream_work: upstream_work,
            _upstream_bare: upstream_bare,
        }
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
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

async fn run_git_async(cwd: &Path, args: &[&str]) {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let output = Command::new("git")
            .current_dir(&cwd)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    })
    .await
    .unwrap();
}

// ── 1. Health endpoint latency ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_health_endpoint_latency() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/healthz", server.addr);
    let iterations = 100;

    // Warm up.
    client.get(&url).send().await.unwrap();

    let start = Instant::now();
    for _ in 0..iterations {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
    }
    let elapsed = start.elapsed();

    let avg = elapsed / iterations;
    eprintln!("healthz: {iterations} calls in {elapsed:?}, avg={avg:?}");
    assert!(
        avg.as_millis() < 50,
        "healthz average latency too high: {avg:?}"
    );
}

// ── 2. Metrics endpoint consistency ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_metrics_endpoint_throughput() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/metrics", server.addr);
    let iterations = 100;

    let start = Instant::now();
    for _ in 0..iterations {
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body = resp.text().await.unwrap();
        assert!(body.contains("git_cache_materialize_total"));
    }
    let elapsed = start.elapsed();

    let avg = elapsed / iterations;
    eprintln!("metrics: {iterations} calls in {elapsed:?}, avg={avg:?}");
    assert!(
        avg.as_millis() < 50,
        "metrics average latency too high: {avg:?}"
    );
}

// ── 3. Sequential clone throughput ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_sequential_clone_throughput() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_count = 5;

    let start = Instant::now();
    let mut clone_dirs = Vec::new();
    for i in 0..clone_count {
        let clone_dir = server.tmp.path().join(format!("perf-clone-{i}"));
        run_git_async(
            server.tmp.path(),
            &[
                "clone",
                "--no-tags",
                "--branch",
                "main",
                &url,
                clone_dir.to_str().unwrap(),
            ],
        )
        .await;
        clone_dirs.push(clone_dir);
    }
    let elapsed = start.elapsed();

    for dir in &clone_dirs {
        let readme = std::fs::read_to_string(dir.join("README.md")).unwrap();
        assert_eq!(readme.trim(), "initial");
    }

    let avg = elapsed / clone_count;
    eprintln!("sequential clone: {clone_count} clones in {elapsed:?}, avg={avg:?}");
    assert!(
        elapsed.as_secs() < 120,
        "sequential clone too slow: {elapsed:?}"
    );
}

// ── 4. Rate limiter throughput ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_rate_limiter_throughput() {
    let server = TestServer::start_with_rate_limit(10).await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/materialize", server.addr);
    let total_requests = 20;

    let body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    let start = Instant::now();
    let mut success_count = 0u32;
    let mut rate_limited_count = 0u32;
    let mut other_count = 0u32;

    for _ in 0..total_requests {
        let resp = client.post(&url).json(&body).send().await.unwrap();

        match resp.status().as_u16() {
            429 => rate_limited_count += 1,
            200 => success_count += 1,
            other => {
                eprintln!("unexpected status: {other}");
                other_count += 1;
            }
        }
    }
    let elapsed = start.elapsed();

    eprintln!(
        "rate limiter: {total_requests} requests in {elapsed:?}, success={success_count}, rate_limited={rate_limited_count}, other={other_count}"
    );

    // With rate_limit=10, we should see some 429s after the first batch.
    assert!(
        rate_limited_count > 0,
        "expected at least some rate-limited responses, got 0 out of {total_requests}"
    );
    assert!(
        success_count > 0,
        "expected at least some successful responses"
    );
    assert_eq!(
        success_count + rate_limited_count + other_count,
        total_requests,
        "all requests should be accounted for"
    );
}
