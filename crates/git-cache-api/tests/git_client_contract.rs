//! Git client contract tests.
//!
//! Runs real `git` CLI commands against the test server and verifies
//! clone, fetch, ls-remote, shallow clone, multi-branch, force-push,
//! large file handling, empty upstream rejection, and metrics.
//! Also contains error response contract tests.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use tokio::net::TcpListener;

struct TestServer {
    addr: SocketAddr,
    tmp: TempDir,
    upstream_work: PathBuf,
    upstream_bare: PathBuf,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_warm_cache(true).await
    }

    async fn start_unwarmed() -> Self {
        Self::start_with_warm_cache(false).await
    }

    async fn start_with_warm_cache(warm_cache: bool) -> Self {
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
            compaction: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            session_cleanup_interval_secs: 300,
            max_concurrent_generation_verifications: 1,
        };

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let server = Self {
            addr,
            tmp,
            upstream_work,
            upstream_bare,
        };
        if warm_cache {
            server.warm_all_heads();
        }
        server
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
        self.warm_all_heads();
        self.head_commit()
    }

    fn metrics_url(&self) -> String {
        format!("http://{}/metrics", self.addr)
    }

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    fn warm_all_heads(&self) {
        let repo_dir = self.tmp.path().join("cache/repos/github.com/org/repo.git");
        if !repo_dir.join("config").exists() {
            std::fs::create_dir_all(repo_dir.parent().unwrap()).unwrap();
            run_git(
                self.tmp.path(),
                &["init", "--bare", repo_dir.to_str().unwrap()],
            );
        }
        run_git(
            &repo_dir,
            &[
                "fetch",
                "--no-tags",
                self.upstream_bare.to_str().unwrap(),
                "+refs/heads/*:refs/cache/upstream/heads/*",
                "+refs/heads/*:refs/heads/*",
            ],
        );
        run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
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

async fn try_git_async(cwd: &Path, args: &[&str]) -> std::process::Output {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new("git")
            .current_dir(&cwd)
            .args(&args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

/// POST JSON to a URL, returning the response.
async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> reqwest::Response {
    client
        .post(url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(body).unwrap())
        .send()
        .await
        .unwrap()
}

// ── Clone / fetch tests ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn cold_direct_clone_reads_through() {
    let server = TestServer::start_unwarmed().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("cold_direct_clone");

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

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());
}

#[tokio::test(flavor = "multi_thread")]
async fn full_clone() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("full_clone");

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

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());

    let readme = std::fs::read_to_string(clone_dir.join("README.md")).unwrap();
    assert_eq!(readme.trim(), "initial");

    let remote_url = git_stdout_async(&clone_dir, &["config", "remote.origin.url"]).await;
    assert_eq!(remote_url, url);
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_with_explicit_branch() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("branch_clone");

    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--branch",
            "main",
            &url,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_clone_depth_1() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("shallow_clone");

    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--depth=1",
            "--branch",
            "main",
            &url,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;

    let count = git_stdout_async(&clone_dir, &["rev-list", "--count", "HEAD"]).await;
    assert_eq!(count, "1", "shallow clone should have exactly 1 commit");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_into_existing_clone() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("fetch_clone");

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

    let first_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    let new_commit = server.commit_and_push("second commit");

    run_git_async(&clone_dir, &["fetch", "origin"]).await;
    let remote_head = git_stdout_async(&clone_dir, &["rev-parse", "origin/main"]).await;

    assert_ne!(first_head, new_commit);
    assert_eq!(remote_head, new_commit);
}

#[tokio::test(flavor = "multi_thread")]
async fn ls_remote_shows_refs() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // Warm the cache.
    let clone_dir = server.tmp.path().join("ls_warm");
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

    let text = git_stdout_async(server.tmp.path(), &["ls-remote", &url]).await;

    assert!(
        text.contains("refs/heads/main"),
        "ls-remote should show refs/heads/main"
    );
    assert!(text.contains("HEAD"), "ls-remote should show HEAD");
    for line in text.lines() {
        assert!(!line.contains("refs/cache"), "internal ref leaked: {line}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_by_exact_sha() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // Warm the cache.
    let clone_dir = server.tmp.path().join("sha_warm");
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
    let head = server.head_commit();

    // Fresh repo fetching by exact SHA.
    let fetch_dir = server.tmp.path().join("sha_fetch");
    std::fs::create_dir_all(&fetch_dir).unwrap();
    run_git_async(&fetch_dir, &["init"]).await;
    run_git_async(&fetch_dir, &["remote", "add", "origin", &url]).await;
    run_git_async(
        &fetch_dir,
        &["fetch", "--no-tags", "--depth=1", "origin", &head],
    )
    .await;
    run_git_async(&fetch_dir, &["checkout", "--detach", &head]).await;

    let fetched_head = git_stdout_async(&fetch_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(fetched_head, head);
}

#[tokio::test(flavor = "multi_thread")]
async fn short_sha_materialize_and_clone() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = server.git_url("github.com/org/repo");

    // Warm the cache.
    let clone_dir = server.tmp.path().join("short_warm");
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
    let head = server.head_commit();
    let short_sha = &head[..8];

    // Materialize by short commit to verify resolution.
    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"short_commit": short_sha}
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    let resolved_commit = body["commit"].as_str().unwrap();
    assert_eq!(
        resolved_commit, head,
        "short SHA should resolve to full commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_branches() {
    let server = TestServer::start().await;

    // Create a feature branch in upstream.
    run_git(&server.upstream_work, &["checkout", "-b", "feature"]);
    std::fs::write(server.upstream_work.join("feature.txt"), "feature\n").unwrap();
    run_git(&server.upstream_work, &["add", "feature.txt"]);
    run_git(&server.upstream_work, &["commit", "-m", "feature commit"]);
    run_git(&server.upstream_work, &["push", "origin", "feature"]);
    run_git(&server.upstream_work, &["checkout", "main"]);
    server.warm_all_heads();

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("multi_branch_clone");

    run_git_async(
        server.tmp.path(),
        &["clone", &url, clone_dir.to_str().unwrap()],
    )
    .await;

    let branches = git_stdout_async(&clone_dir, &["branch", "-r"]).await;
    assert!(branches.contains("origin/main"), "should see origin/main");
    assert!(
        branches.contains("origin/feature"),
        "should see origin/feature"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn force_push_handling() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("force_push_clone");

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

    let first_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;

    // Force push rewritten history.
    run_git(&server.upstream_work, &["checkout", "--orphan", "rewrite"]);
    std::fs::write(server.upstream_work.join("README.md"), "rewritten\n").unwrap();
    run_git(&server.upstream_work, &["add", "README.md"]);
    run_git(&server.upstream_work, &["commit", "-m", "rewritten"]);
    run_git(&server.upstream_work, &["branch", "-M", "main"]);
    run_git(
        &server.upstream_work,
        &["push", "--force", "origin", "main"],
    );
    server.warm_all_heads();
    let new_head = server.head_commit();

    run_git_async(&clone_dir, &["fetch", "origin"]).await;
    let fetched_head = git_stdout_async(&clone_dir, &["rev-parse", "origin/main"]).await;

    assert_ne!(first_head, new_head);
    assert_eq!(fetched_head, new_head);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_upstream_rejection() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/nonexistent/upstream");
    let clone_dir = server.tmp.path().join("empty_clone");

    let output = try_git_async(
        server.tmp.path(),
        &["clone", &url, clone_dir.to_str().unwrap()],
    )
    .await;

    assert!(
        !output.status.success(),
        "cloning non-existent upstream should fail"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn large_file_handling() {
    let server = TestServer::start().await;

    // Create a 5MB file in upstream.
    let data: Vec<u8> = (0..5 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
    let checksum_original = simple_checksum(&data);
    std::fs::write(server.upstream_work.join("large.bin"), &data).unwrap();
    run_git(&server.upstream_work, &["add", "large.bin"]);
    run_git(&server.upstream_work, &["commit", "-m", "add large file"]);
    run_git(
        &server.upstream_work,
        &["push", "--force", "origin", "main"],
    );
    server.warm_all_heads();

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("large_clone");

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

    let cloned_data = std::fs::read(clone_dir.join("large.bin")).unwrap();
    let checksum_cloned = simple_checksum(&cloned_data);
    assert_eq!(
        checksum_original, checksum_cloned,
        "large file checksum mismatch"
    );
    assert_eq!(cloned_data.len(), 5 * 1024 * 1024);
}

fn simple_checksum(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &b| acc.wrapping_add(b as u64))
}

// ── Metrics contract ────────────────────────────────────────────────────

fn parse_metric(body: &str, name: &str) -> u64 {
    for line in body.lines() {
        if line.starts_with(name) {
            let value = line.split_whitespace().last().unwrap();
            return value.parse().unwrap();
        }
    }
    panic!("metric {name} not found in:\n{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_increment_on_materialize() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let m0 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mat_before = parse_metric(&m0, "git_cache_materialize_total");

    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);

    let m1 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mat_after = parse_metric(&m1, "git_cache_materialize_total");

    assert!(
        mat_after > mat_before,
        "materialize_total should increment: before={mat_before}, after={mat_after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_increment_on_git_remote() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let m0 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let refs_before = parse_metric(&m0, "git_cache_git_remote_refs_total");
    let pack_before = parse_metric(&m0, "git_cache_git_remote_upload_pack_total");

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("metrics_clone");
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

    let m1 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let refs_after = parse_metric(&m1, "git_cache_git_remote_refs_total");
    let pack_after = parse_metric(&m1, "git_cache_git_remote_upload_pack_total");

    assert!(
        refs_after > refs_before,
        "git_remote_refs_total should increment: before={refs_before}, after={refs_after}"
    );
    assert!(
        pack_after > pack_before,
        "git_remote_upload_pack_total should increment: before={pack_before}, after={pack_after}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_increment_on_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let m0 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let err_before = parse_metric(&m0, "git_cache_materialize_errors_total");

    let _resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/nonexistent",
            "selector": {"branch": "main"}
        }),
    )
    .await;

    let m1 = client
        .get(server.metrics_url())
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let err_after = parse_metric(&m1, "git_cache_materialize_errors_total");

    assert!(
        err_after > err_before,
        "materialize_errors_total should increment: before={err_before}, after={err_after}"
    );
}

// ── Error response contract ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn error_for_nonexistent_repo() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/nonexistent",
            "selector": {"branch": "main"}
        }),
    )
    .await;

    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected error for non-existent repo, got {}",
        resp.status()
    );
    let body: serde_json::Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    assert!(body.get("error").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn error_400_for_malformed_json() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .header("content-type", "application/json")
        .body("{not valid json")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn error_400_for_invalid_repo_key() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "invalid",
            "selector": {"branch": "main"}
        }),
    )
    .await;

    assert!(
        resp.status() == 400 || resp.status() == 422,
        "expected 400 or 422 for invalid repo key, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn error_405_for_unsupported_methods() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let put_resp = client
        .put(server.materialize_url())
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(put_resp.status(), 405);

    let delete_resp = client
        .delete(server.materialize_url())
        .send()
        .await
        .unwrap();
    assert_eq!(delete_resp.status(), 405);
}

#[tokio::test(flavor = "multi_thread")]
async fn error_body_has_json_structure() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // Use a valid repo key that doesn't exist upstream (to get a proper API error, not a deserialization error).
    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/nonexistent",
            "selector": {"branch": "main"}
        }),
    )
    .await;

    let text = resp.text().await.unwrap();
    let body: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("response is not valid JSON: {e}\nbody: {text}"));
    assert!(
        body.get("error").is_some(),
        "error response should have 'error' field: {body}"
    );
    assert!(body["error"].is_string());
}
