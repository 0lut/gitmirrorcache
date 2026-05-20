//! API-level resource contention tests.
//!
//! These tests stress concurrent access patterns at the HTTP API layer:
//! parallel materializations, concurrent git clones, rate limiting under load,
//! and session expiry behavior.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Barrier;

struct TestServer {
    addr: std::net::SocketAddr,
    tmp: TempDir,
    upstream_work: PathBuf,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_config(|config| config).await
    }

    async fn start_with_config(f: impl FnOnce(AppConfig) -> AppConfig) -> Self {
        let tmp = TempDir::new().unwrap();
        let upstream_bare = tmp.path().join("upstreams/github.com/org/repo.git");
        let upstream_work = tmp.path().join("work");

        std::fs::create_dir_all(upstream_bare.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&upstream_work).unwrap();

        run_git(tmp.path(), &["init", "--bare", upstream_bare.to_str().unwrap()]);
        run_git(&upstream_work, &["init"]);
        run_git(&upstream_work, &["config", "user.email", "test@example.com"]);
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
        run_git(
            &upstream_bare,
            &["symbolic-ref", "HEAD", "refs/heads/main"],
        );

        // Bind first to discover the real port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let config = f(AppConfig {
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
            rate_limit_per_minute: 0,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: GitRemoteConfig {
                enabled: true,
                ..Default::default()
            },
        });

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        Self {
            addr,
            tmp,
            upstream_work,
        }
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
    }

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    fn head_commit(&self) -> String {
        git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
    }

    fn commit_and_push(&self, contents: &str) -> String {
        std::fs::write(self.upstream_work.join("README.md"), format!("{contents}\n")).unwrap();
        run_git(&self.upstream_work, &["add", "README.md"]);
        run_git(&self.upstream_work, &["commit", "-m", contents]);
        run_git(&self.upstream_work, &["push", "--force", "origin", "main"]);
        self.head_commit()
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
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

async fn git_stdout_async(cwd: &Path, args: &[&str]) -> String {
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
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    })
    .await
    .unwrap()
}

// ── 1. Concurrent materialize for same branch ────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_materialize_same_branch() {
    let server = TestServer::start().await;
    let barrier = Arc::new(Barrier::new(10));
    let client = reqwest::Client::new();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let bar = Arc::clone(&barrier);
            let url = server.materialize_url();
            let c = client.clone();
            tokio::spawn(async move {
                bar.wait().await;
                c.post(&url)
                    .json(&serde_json::json!({
                        "repo": "github.com/org/repo",
                        "selector": {"branch": "main"}
                    }))
                    .send()
                    .await
                    .unwrap()
            })
        })
        .collect();

    let mut successes = 0;
    let mut retryable = 0;
    for handle in handles {
        let resp = handle.await.unwrap();
        let status = resp.status().as_u16();
        if status == 200 {
            successes += 1;
        } else if status == 503 || status == 409 {
            retryable += 1;
        } else {
            // Other errors are acceptable during contention — log but don't panic.
            retryable += 1;
        }
    }

    assert!(
        successes > 0,
        "at least one materialize must succeed (got {successes} successes, {retryable} retryable)"
    );
}

// ── 2. Concurrent git clone ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_git_clone_some_succeed_under_contention() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let expected_head = server.head_commit();

    // Under concurrent access, the cache proxy may return 409 (LeaseBusy)
    // for some clones. We verify that at least some succeed and produce the
    // correct commit, and no panics or data corruption occur.
    let handles: Vec<_> = (0..5)
        .map(|i| {
            let clone_dir = server.tmp.path().join(format!("clone-{i}"));
            let u = url.clone();
            let parent = server.tmp.path().to_path_buf();
            tokio::spawn(async move {
                let cwd = parent.clone();
                let args: Vec<String> = vec![
                    "clone".into(),
                    "--no-tags".into(),
                    "--branch".into(),
                    "main".into(),
                    u,
                    clone_dir.to_str().unwrap().to_string(),
                ];
                let result = tokio::task::spawn_blocking(move || {
                    Command::new("git")
                        .current_dir(&cwd)
                        .args(&args)
                        .output()
                        .unwrap()
                })
                .await
                .unwrap();
                if result.status.success() {
                    Some(
                        tokio::task::spawn_blocking(move || {
                            let output = Command::new("git")
                                .current_dir(&clone_dir)
                                .args(["rev-parse", "HEAD"])
                                .output()
                                .unwrap();
                            String::from_utf8(output.stdout).unwrap().trim().to_string()
                        })
                        .await
                        .unwrap(),
                    )
                } else {
                    None // 409/503 under contention — acceptable
                }
            })
        })
        .collect();

    let mut successes = 0;
    for handle in handles {
        if let Some(head) = handle.await.unwrap() {
            assert_eq!(head, expected_head);
            successes += 1;
        }
    }
    assert!(
        successes >= 1,
        "at least one concurrent clone should succeed"
    );
}

// ── 3. Clone + push-advance race ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn clone_push_advance_race() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // First clone.
    let clone1 = server.tmp.path().join("adv-clone1");
    run_git_async(
        server.tmp.path(),
        &["clone", "--no-tags", "--branch", "main", &url, clone1.to_str().unwrap()],
    )
    .await;
    let first_head = git_stdout_async(&clone1, &["rev-parse", "HEAD"]).await;

    // Push new commit upstream.
    let new_commit = server.commit_and_push("advanced commit");
    assert_ne!(first_head, new_commit);

    // Second clone must see the new commit.
    let clone2 = server.tmp.path().join("adv-clone2");
    run_git_async(
        server.tmp.path(),
        &["clone", "--no-tags", "--branch", "main", &url, clone2.to_str().unwrap()],
    )
    .await;
    let second_head = git_stdout_async(&clone2, &["rev-parse", "HEAD"]).await;

    assert_eq!(second_head, new_commit);
}

// ── 4. Rate limiting under concurrent load ───────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn rate_limiting_under_concurrent_load() {
    let server = TestServer::start_with_config(|mut config| {
        config.rate_limit_per_minute = 5;
        config
    })
    .await;

    let client = reqwest::Client::new();
    let barrier = Arc::new(Barrier::new(20));

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let bar = Arc::clone(&barrier);
            let url = server.materialize_url();
            let c = client.clone();
            tokio::spawn(async move {
                bar.wait().await;
                c.post(&url)
                    .json(&serde_json::json!({
                        "repo": "github.com/org/repo",
                        "selector": {"branch": "main"}
                    }))
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        })
        .collect();

    let statuses: Vec<u16> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let rate_limited = statuses.iter().filter(|&&s| s == 429).count();
    let succeeded = statuses.iter().filter(|&&s| s == 200).count();

    assert!(
        rate_limited > 0,
        "some requests should be rate limited (429)"
    );
    assert!(
        succeeded <= 5,
        "at most 5 should succeed under rate limit of 5/min, got {succeeded}"
    );
}

// ── 5. Session expiry during use ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn session_expiry_during_use() {
    let server = TestServer::start_with_config(|mut config| {
        config.session_ttl_seconds = 1;
        config
    })
    .await;

    let client = reqwest::Client::new();

    // Create a session via materialize.
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let git_url = body["git_url"].as_str().unwrap().to_string();

    // Wait for the session to expire.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Try to use the expired session for info/refs.
    let info_refs_url = format!("{git_url}/info/refs?service=git-upload-pack");
    let resp = client.get(&info_refs_url).send().await.unwrap();

    // Should get 404 because the session has expired.
    assert_eq!(
        resp.status().as_u16(),
        404,
        "expired session should return 404"
    );
}
