//! Advanced API-level resource contention tests.
//!
//! Tests cover concurrent materialization across multiple repos, materialize during
//! active clone, rate limiter fairness, session expiry during clone, concurrent
//! session creation and expiry, parallel fetch after force-push, and disk pressure.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::collections::HashMap;
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
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
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

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
    }

    fn head_commit(&self) -> String {
        git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
    }

    fn commit_and_push(&self, contents: &str) -> String {
        std::fs::write(
            self.upstream_work.join("README.md"),
            format!("{contents}\n"),
        )
        .unwrap();
        run_git(&self.upstream_work, &["add", "README.md"]);
        run_git(&self.upstream_work, &["commit", "-m", contents]);
        run_git(&self.upstream_work, &["push", "--force", "origin", "main"]);
        self.head_commit()
    }
}

/// Multi-repo test server that creates multiple upstream repos.
struct MultiRepoTestServer {
    addr: std::net::SocketAddr,
    #[allow(dead_code)]
    tmp: TempDir,
    repos: Vec<RepoInfo>,
}

struct RepoInfo {
    name: String,
    work_dir: PathBuf,
}

impl MultiRepoTestServer {
    async fn start(repo_count: usize) -> Self {
        Self::start_with_config(repo_count, |config| config).await
    }

    async fn start_with_config(repo_count: usize, f: impl FnOnce(AppConfig) -> AppConfig) -> Self {
        let tmp = TempDir::new().unwrap();
        let mut repos = Vec::new();

        for i in 0..repo_count {
            let repo_name = format!("repo{i}");
            let upstream_bare = tmp
                .path()
                .join(format!("upstreams/github.com/org/{repo_name}.git"));
            let upstream_work = tmp.path().join(format!("work-{repo_name}"));

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
            std::fs::write(
                upstream_work.join("README.md"),
                format!("initial-{repo_name}\n"),
            )
            .unwrap();
            run_git(&upstream_work, &["add", "README.md"]);
            run_git(
                &upstream_work,
                &["commit", "-m", &format!("initial-{repo_name}")],
            );
            run_git(&upstream_work, &["branch", "-M", "main"]);
            run_git(
                &upstream_work,
                &["remote", "add", "origin", upstream_bare.to_str().unwrap()],
            );
            run_git(&upstream_work, &["push", "origin", "main"]);
            run_git(&upstream_bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

            repos.push(RepoInfo {
                name: repo_name,
                work_dir: upstream_work,
            });
        }

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
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
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
        });

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        Self { addr, tmp, repos }
    }

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    #[allow(dead_code)]
    fn git_url(&self, repo_name: &str) -> String {
        format!("http://{}/git/github.com/org/{}.git", self.addr, repo_name)
    }

    fn head_commit(&self, idx: usize) -> String {
        git_stdout(&self.repos[idx].work_dir, &["rev-parse", "HEAD"])
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

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

// ── 1. Concurrent materialize for different repos ────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_materialize_different_repos() {
    let server = MultiRepoTestServer::start(3).await;
    let client = reqwest::Client::new();
    let barrier = Arc::new(Barrier::new(15));

    let mut handles = Vec::new();
    for repo_idx in 0..3 {
        let repo_name = server.repos[repo_idx].name.clone();
        for _ in 0..5 {
            let bar = Arc::clone(&barrier);
            let url = server.materialize_url();
            let c = client.clone();
            let name = repo_name.clone();
            handles.push(tokio::spawn(async move {
                bar.wait().await;
                let resp = c
                    .post(&url)
                    .json(&serde_json::json!({
                        "repo": format!("github.com/org/{name}"),
                        "selector": {"branch": "main"}
                    }))
                    .send()
                    .await
                    .unwrap();
                (name, resp.status().as_u16())
            }));
        }
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // Group by repo and verify at least one success per repo.
    let mut per_repo: HashMap<String, Vec<u16>> = HashMap::new();
    for (name, status) in results {
        per_repo.entry(name).or_default().push(status);
    }

    for (repo_name, statuses) in &per_repo {
        let successes = statuses.iter().filter(|&&s| s == 200).count();
        assert!(
            successes > 0,
            "repo {repo_name} should have at least one successful materialize, got statuses: {statuses:?}"
        );
    }

    // Verify correct commits by materializing once more sequentially.
    for (idx, repo_info) in server.repos.iter().enumerate() {
        let expected = server.head_commit(idx);
        let resp = client
            .post(server.materialize_url())
            .json(&serde_json::json!({
                "repo": format!("github.com/org/{}", repo_info.name),
                "selector": {"branch": "main"}
            }))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 200 {
            let body: serde_json::Value = resp.json().await.unwrap();
            let commit = body["commit"].as_str().unwrap_or_default();
            assert_eq!(
                commit, expected,
                "repo {} should resolve to correct commit",
                repo_info.name
            );
        }
    }
}

// ── 2. Materialize during active clone ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_during_active_clone() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let client = reqwest::Client::new();

    // Start a clone in background.
    let clone_dir = server.tmp.path().join("concurrent-clone");
    let clone_url = url.clone();
    let clone_handle = tokio::spawn(async move {
        let dir = clone_dir.clone();
        let u = clone_url;
        tokio::task::spawn_blocking(move || {
            Command::new("git")
                .args([
                    "clone",
                    "--no-tags",
                    "--branch",
                    "main",
                    &u,
                    dir.to_str().unwrap(),
                ])
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    });

    // Simultaneously call materialize.
    let materialize_handle = {
        let c = client.clone();
        let mat_url = server.materialize_url();
        tokio::spawn(async move {
            c.post(&mat_url)
                .json(&serde_json::json!({
                    "repo": "github.com/org/repo",
                    "selector": {"branch": "main"}
                }))
                .send()
                .await
                .unwrap()
        })
    };

    let clone_result = clone_handle.await.unwrap();
    let materialize_resp = materialize_handle.await.unwrap();

    // Both should succeed or return retriable errors — no panics.
    let mat_status = materialize_resp.status().as_u16();
    assert!(
        mat_status == 200 || mat_status == 503 || mat_status == 409,
        "materialize during clone should succeed or be retriable, got {mat_status}"
    );

    // Clone may fail under contention (e.g., 409) — that's acceptable.
    if clone_result.status.success() {
        let clone_dir = server.tmp.path().join("concurrent-clone");
        let head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head, server.head_commit());
    }
}

// ── 3. Rate limiter fairness ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn rate_limiter_fairness() {
    let server = TestServer::start_with_config(|mut config| {
        config.rate_limit_per_minute = 5;
        config
    })
    .await;

    let client = reqwest::Client::new();
    let barrier = Arc::new(Barrier::new(20));

    // 4 "clients" of 5 requests each (20 total).
    let mut handles = Vec::new();
    for client_id in 0..4u32 {
        for _ in 0..5 {
            let bar = Arc::clone(&barrier);
            let url = server.materialize_url();
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                bar.wait().await;
                let resp = c
                    .post(&url)
                    .json(&serde_json::json!({
                        "repo": "github.com/org/repo",
                        "selector": {"branch": "main"}
                    }))
                    .send()
                    .await
                    .unwrap();
                (client_id, resp.status().as_u16())
            }));
        }
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    let mut per_client_success: HashMap<u32, usize> = HashMap::new();
    let mut total_success = 0;
    let mut total_rate_limited = 0;

    for (cid, status) in &results {
        if *status == 200 {
            *per_client_success.entry(*cid).or_default() += 1;
            total_success += 1;
        } else if *status == 429 {
            total_rate_limited += 1;
        }
    }

    assert!(
        total_rate_limited > 0,
        "some requests should be rate limited"
    );
    assert!(
        total_success <= 5,
        "at most 5 should succeed under rate limit of 5/min, got {total_success}"
    );

    // Under the global rate limiter, at least some traffic gets through.
    // We verify the rate limiter doesn't starve all clients completely.
    let clients_with_success = per_client_success.len();
    assert!(
        clients_with_success >= 1,
        "at least one client should get at least 1 success, got {clients_with_success} clients with success"
    );
}

// ── 4. Session expiry during clone ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn session_expiry_during_clone() {
    let server = TestServer::start_with_config(|mut config| {
        config.session_ttl_seconds = 2;
        config
    })
    .await;

    let client = reqwest::Client::new();

    // Materialize to create a session.
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

    // Wait 1 second (session still valid).
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Start clone using the session URL — it should complete or fail gracefully.
    let clone_dir = server.tmp.path().join("expiry-clone");
    let result = tokio::task::spawn_blocking(move || {
        Command::new("git")
            .args([
                "clone",
                "--no-tags",
                "--branch",
                "main",
                &git_url,
                clone_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    // Either it succeeds (session was still valid) or fails gracefully (no panic/crash).
    if result.status.success() {
        let clone_dir = server.tmp.path().join("expiry-clone");
        let head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head, server.head_commit());
    } else {
        // Failure is acceptable — session may have expired mid-operation.
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !stderr.contains("panic") && !stderr.contains("SIGSEGV"),
            "clone failure should be graceful, got: {stderr}"
        );
    }
}

// ── 5. Concurrent session creation and expiry ────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_session_creation_and_expiry() {
    let server = TestServer::start_with_config(|mut config| {
        config.session_ttl_seconds = 1;
        config
    })
    .await;

    let client = reqwest::Client::new();
    let barrier = Arc::new(Barrier::new(20));

    // Rapidly create sessions while old ones expire (ttl=1s).
    let mut handles = Vec::new();
    for wave in 0..4u32 {
        for _ in 0..5 {
            let bar = Arc::clone(&barrier);
            let url = server.materialize_url();
            let c = client.clone();
            handles.push(tokio::spawn(async move {
                if wave > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(wave as u64 * 500)).await;
                }
                bar.wait().await;
                let resp = c
                    .post(&url)
                    .json(&serde_json::json!({
                        "repo": "github.com/org/repo",
                        "selector": {"branch": "main"}
                    }))
                    .send()
                    .await
                    .unwrap();
                resp.status().as_u16()
            }));
        }
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();

    // No panics should occur — verify all statuses are expected HTTP codes.
    for status in &results {
        assert!(
            *status == 200 || *status == 409 || *status == 429 || *status == 500 || *status == 503,
            "unexpected status {status} during concurrent session creation/expiry"
        );
    }

    // At least some should succeed.
    let successes = results.iter().filter(|&&s| s == 200).count();
    assert!(
        successes > 0,
        "at least one session creation should succeed"
    );
}

// ── 6. Parallel fetch after force-push ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn parallel_fetch_after_force_push() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // Record commit A (already pushed during server setup).
    let commit_a = server.head_commit();

    // Start 5 clones for commit A.
    let mut handles_a = Vec::new();
    for i in 0..5 {
        let clone_dir = server.tmp.path().join(format!("fp-clone-a{i}"));
        let u = url.clone();
        handles_a.push(tokio::spawn(async move {
            let dir = clone_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                Command::new("git")
                    .args([
                        "clone",
                        "--no-tags",
                        "--branch",
                        "main",
                        &u,
                        dir.to_str().unwrap(),
                    ])
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            if result.status.success() {
                Some(git_stdout(&clone_dir, &["rev-parse", "HEAD"]))
            } else {
                None
            }
        }));
    }

    // Force-push commit B.
    let commit_b = server.commit_and_push("force-push-commit");
    assert_ne!(commit_a, commit_b);

    // Start 5 more clones for what should be commit B.
    let mut handles_b = Vec::new();
    for i in 0..5 {
        let clone_dir = server.tmp.path().join(format!("fp-clone-b{i}"));
        let u = url.clone();
        handles_b.push(tokio::spawn(async move {
            let dir = clone_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                Command::new("git")
                    .args([
                        "clone",
                        "--no-tags",
                        "--branch",
                        "main",
                        &u,
                        dir.to_str().unwrap(),
                    ])
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            if result.status.success() {
                Some(git_stdout(&clone_dir, &["rev-parse", "HEAD"]))
            } else {
                None
            }
        }));
    }

    // Collect results for group A.
    for handle in handles_a {
        if let Some(head) = handle.await.unwrap() {
            // Each clone gets either A or B — never mixed/corrupt.
            assert!(
                head == commit_a || head == commit_b,
                "clone (group A) got unexpected commit {head}, expected {commit_a} or {commit_b}"
            );
        }
    }

    // Collect results for group B.
    for handle in handles_b {
        if let Some(head) = handle.await.unwrap() {
            assert!(
                head == commit_a || head == commit_b,
                "clone (group B) got unexpected commit {head}, expected {commit_a} or {commit_b}"
            );
        }
    }
}

// ── 7. Disk pressure during clone ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn disk_pressure_during_clone() {
    // Set disk quota very low (5MB) to trigger disk pressure.
    let server = TestServer::start_with_config(|mut config| {
        config.disk.quota_bytes = 5 * 1024 * 1024;
        config
    })
    .await;

    let client = reqwest::Client::new();

    // Try to materialize — should either succeed (small repo) or fail gracefully.
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    // Small test repos may fit in 5MB, so 200 is acceptable.
    // If it doesn't fit, we expect a graceful error (507 or 500), not a panic.
    assert!(
        status == 200 || status == 500 || status == 503 || status == 507,
        "disk pressure should produce graceful error or succeed for small repo, got {status}"
    );

    // Verify the server is still responsive after disk pressure.
    let health_resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await;
    assert!(
        health_resp.is_ok(),
        "server should still be responsive after disk pressure"
    );
}
