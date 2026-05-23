//! Correctness integration tests for git-cache-api endpoints.
//!
//! Tests edge cases for materialize, resolve, healthz, metrics, and session
//! endpoints using the same TestServer pattern as other integration tests.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;
use tokio::net::TcpListener;

// ── Test server helper ──────────────────────────────────────────────────

struct TestServer {
    addr: SocketAddr,
    _tmp: TempDir,
}

impl TestServer {
    async fn start() -> Self {
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
        };

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        Self { addr, _tmp: tmp }
    }

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    fn resolve_url(&self) -> String {
        format!("http://{}/v1/resolve", self.addr)
    }

    fn healthz_url(&self) -> String {
        format!("http://{}/healthz", self.addr)
    }

    fn metrics_url(&self) -> String {
        format!("http://{}/metrics", self.addr)
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

// ── Healthz endpoint ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn healthz_returns_ok_json() {
    let server = TestServer::start().await;
    let resp = reqwest::get(&server.healthz_url()).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["checked_at"].is_string());
}

// ── Metrics endpoint ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn metrics_returns_expected_counter_names() {
    let server = TestServer::start().await;
    let resp = reqwest::get(&server.metrics_url()).await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    for counter in &[
        "git_cache_materialize_total",
        "git_cache_materialize_errors_total",
        "git_cache_git_upload_pack_total",
        "git_cache_rate_limited_total",
        "git_cache_git_remote_refs_total",
        "git_cache_git_remote_upload_pack_total",
    ] {
        assert!(body.contains(counter), "missing counter: {counter}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_content_type_is_text_plain() {
    let server = TestServer::start().await;
    let resp = reqwest::get(&server.metrics_url()).await.unwrap();
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(ct.contains("text/plain"));
}

// ── Materialize endpoint edge cases ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_missing_repo_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_invalid_repo_key_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "invalid-repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_empty_selector_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_two_selectors_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main", "default_branch": true}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error(),
        "expected 4xx, got {}",
        resp.status()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_nonexistent_repo_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/nonexistent/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    // Could be 404 or 500 depending on upstream handling
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected error status, got {}",
        resp.status()
    );
}

// ── Resolve endpoint (same as materialize) ──────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn resolve_with_valid_branch_works_like_materialize() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.resolve_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["repo"], "github.com/org/repo");
    assert!(body["commit"].is_string());
    assert!(body["git_url"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_missing_repo_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let resp = client
        .post(server.resolve_url())
        .json(&serde_json::json!({
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error());
}

// ── Materialize same repo twice returns same commit ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_same_repo_twice_returns_same_commit() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    let resp1 = client
        .post(server.materialize_url())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status(), 200);
    let json1: serde_json::Value = resp1.json().await.unwrap();

    let resp2 = client
        .post(server.materialize_url())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status(), 200);
    let json2: serde_json::Value = resp2.json().await.unwrap();

    assert_eq!(json1["commit"], json2["commit"]);
}

// ── DefaultBranch selector works ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_default_branch_selector_works() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"default_branch": true}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["repo"], "github.com/org/repo");
    assert!(body["commit"].is_string());
}

// ── Branch selector with nonexistent branch returns error ───────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_nonexistent_branch_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "nonexistent-branch-xyz"}
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected error for nonexistent branch, got {}",
        resp.status()
    );
}

// ── Session clone with wrong repo in URL ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn session_clone_wrong_repo_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // First materialize to get a session
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let git_url = body["git_url"].as_str().unwrap();

    // Extract session ID from git_url (format: http://host/git/session/{id}/{repo}.git)
    let session_id = git_url
        .split("/git/session/")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap();

    // Try to access session with wrong repo
    let wrong_url = format!(
        "http://{}/git/session/{}/github.com/wrong/repo.git/info/refs?service=git-upload-pack",
        server.addr, session_id
    );
    let resp = reqwest::get(&wrong_url).await.unwrap();
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "expected error for wrong repo in session URL, got {}",
        resp.status()
    );
}

// ── Materialize with invalid JSON body ──────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_with_invalid_json_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .header("content-type", "application/json")
        .body("not json at all")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error());
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_with_empty_body_returns_error() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .header("content-type", "application/json")
        .body("")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error());
}

// ── Materialize response shape validation ───────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn materialize_response_has_expected_fields() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["repo"].is_string());
    assert!(body["commit"].is_string());
    assert!(body["source"].is_string());
    assert!(body["verified_at"].is_string());
    assert!(body["git_url"].is_string());
    assert!(body["ref"].is_string());
    assert!(body["expires_at"].is_string());

    // commit should be 40-char hex
    let commit = body["commit"].as_str().unwrap();
    assert_eq!(commit.len(), 40);
    assert!(commit.chars().all(|c| c.is_ascii_hexdigit()));
}
