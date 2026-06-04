//! Session-based Smart HTTP protocol tests.
//!
//! Verifies the full materialize → session URL → git clone lifecycle,
//! expired session handling, session mismatch, invalid session IDs,
//! and receive-pack rejection on session URLs.

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
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_ttl(3600).await
    }

    async fn start_with_ttl(session_ttl_seconds: u64) -> Self {
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

        let config = AppConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            public_base_url: "http://127.0.0.1:0".into(),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 120,
            max_git_output_bytes: 64 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            session_ttl_seconds,
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

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let config = AppConfig {
            public_base_url: format!("http://{addr}"),
            ..config
        };

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

    fn head_commit(&self) -> String {
        git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
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

async fn run_git_with_extra_header_async(cwd: &Path, args: &[&str], header: String) {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let output = Command::new("git")
            .current_dir(&cwd)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.extraHeader")
            .env("GIT_CONFIG_VALUE_0", header)
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

#[derive(serde::Deserialize)]
struct MaterializeResponse {
    git_url: String,
    commit: String,
    #[serde(rename = "ref")]
    ref_name: String,
    session_token: Option<String>,
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn full_session_lifecycle() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    // 1. Materialize
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

    let body = resp.text().await.unwrap();
    let mat: MaterializeResponse = serde_json::from_str(&body).unwrap();
    assert!(!mat.git_url.is_empty());
    assert!(!mat.commit.is_empty());
    assert!(mat.ref_name.starts_with("refs/cache/sessions/"));

    // 2. Ref advertisement via session URL
    let refs_url = format!("{}/info/refs?service=git-upload-pack", mat.git_url);
    let refs_resp = client.get(&refs_url).send().await.unwrap();
    assert_eq!(refs_resp.status(), 200);
    let refs_body = refs_resp.text().await.unwrap();
    assert!(
        refs_body.starts_with("001e# service=git-upload-pack"),
        "ref advertisement should start with service preamble"
    );

    // 3. Session ref should be in the advertisement
    assert!(
        refs_body.contains(&mat.ref_name),
        "session ref {} not found in advertisement",
        mat.ref_name
    );

    // 4. Upload-pack POST with the session commit
    let sha = &mat.commit;
    let want_line = format!("want {sha} multi_ack_detailed side-band-64k thin-pack ofs-delta\n");
    let pkt_want = format!("{:04x}{}", 4 + want_line.len(), want_line);
    let pack_body = format!("{pkt_want}00000009done\n");

    let pack_url = format!("{}/git-upload-pack", mat.git_url);
    let pack_resp = client
        .post(&pack_url)
        .header("Content-Type", "application/x-git-upload-pack-request")
        .body(pack_body)
        .send()
        .await
        .unwrap();
    assert_eq!(pack_resp.status(), 200);
    let pack_bytes = pack_resp.bytes().await.unwrap();
    let has_nak = pack_bytes.windows(3).any(|w| w == b"NAK");
    let has_pack = pack_bytes.windows(4).any(|w| w == b"PACK");
    assert!(
        has_nak,
        "upload-pack response should contain negotiation data"
    );
    assert!(
        has_pack,
        "upload-pack response should contain pack data, got {} bytes",
        pack_bytes.len()
    );

    // 5. Actual git clone using session URL
    let clone_dir = server.tmp.path().join("session_clone");
    run_git_async(
        server.tmp.path(),
        &["clone", &mat.git_url, clone_dir.to_str().unwrap()],
    )
    .await;

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_upload_pack_accepts_large_request_body() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

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
    let mat: MaterializeResponse = serde_json::from_str(&resp.text().await.unwrap()).unwrap();

    let sha = &mat.commit;
    let want_line = format!("want {sha}\n");
    let mut pack_body = format!("{:04x}{want_line}0000", 4 + want_line.len()).into_bytes();
    let have_line = format!("have {sha}\n");
    let have_pkt = format!("{:04x}{have_line}", 4 + have_line.len());
    while pack_body.len() <= 2 * 1024 * 1024 {
        pack_body.extend_from_slice(have_pkt.as_bytes());
    }
    pack_body.extend_from_slice(b"0009done\n");

    let pack_url = format!("{}/git-upload-pack", mat.git_url);
    let pack_resp = client
        .post(&pack_url)
        .header("Content-Type", "application/x-git-upload-pack-request")
        .body(pack_body)
        .send()
        .await
        .unwrap();

    assert_ne!(
        pack_resp.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "git upload-pack requests over Axum's default 2 MiB body limit should reach the handler"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn authenticated_materialize_session_requires_bearer_token() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .header("content-type", "application/json")
        .header(
            "Git-Cache-Upstream-Authorization",
            "Basic dXNlcjpwYXNzd29yZA==",
        )
        .body(
            serde_json::to_string(&serde_json::json!({
                "repo": "github.com/org/repo",
                "selector": {"branch": "main"},
                "upstream_authorization": "required"
            }))
            .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let mat: MaterializeResponse = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
    let token = mat
        .session_token
        .as_deref()
        .expect("authenticated materialize should return session_token");

    let refs_url = format!("{}/info/refs?service=git-upload-pack", mat.git_url);
    let missing = client.get(&refs_url).send().await.unwrap();
    assert_eq!(missing.status(), 401);

    let wrong = client
        .get(&refs_url)
        .header("Authorization", "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let refs_resp = client
        .get(&refs_url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(refs_resp.status(), 200);
    let refs_body = refs_resp.text().await.unwrap();
    assert!(refs_body.contains(&mat.ref_name));
    assert!(
        !refs_body.contains(" filter "),
        "protected session should not advertise filter capability"
    );

    let clone_dir = server.tmp.path().join("protected_session_clone");
    run_git_with_extra_header_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            &mat.git_url,
            clone_dir.to_str().unwrap(),
        ],
        format!("Authorization: Bearer {token}"),
    )
    .await;
    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(
        cloned_head, mat.commit,
        "protected session clone should fetch the authorized commit"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_session_returns_404() {
    let server = TestServer::start_with_ttl(1).await;
    let client = reqwest::Client::new();

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
    let mat: MaterializeResponse = serde_json::from_str(&resp.text().await.unwrap()).unwrap();

    // Wait for session to expire.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let refs_url = format!("{}/info/refs?service=git-upload-pack", mat.git_url);
    let resp = client.get(&refs_url).send().await.unwrap();
    assert_eq!(resp.status(), 404, "expired session should return 404");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_session_id_returns_400() {
    let server = TestServer::start().await;
    let url = format!(
        "http://{}/git/session/not-a-uuid/github.com/org/repo.git/info/refs?service=git-upload-pack",
        server.addr
    );
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_receive_pack_rejected() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }),
    )
    .await;
    let mat: MaterializeResponse = serde_json::from_str(&resp.text().await.unwrap()).unwrap();

    let url = format!("{}/info/refs?service=git-receive-pack", mat.git_url);
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 405);

    let url = format!("{}/git-receive-pack", mat.git_url);
    let resp = client.post(&url).body(b"" as &[u8]).send().await.unwrap();
    assert_eq!(resp.status(), 405);
}

#[tokio::test(flavor = "multi_thread")]
async fn session_repo_mismatch_fails() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let resp = post_json(
        &client,
        &server.materialize_url(),
        &serde_json::json!({
            "repo": "github.com/org/repo",
            "selector": {"branch": "main"}
        }),
    )
    .await;
    let mat: MaterializeResponse = serde_json::from_str(&resp.text().await.unwrap()).unwrap();

    // Extract session ID from git_url.
    let session_id = mat
        .git_url
        .split("/git/session/")
        .nth(1)
        .unwrap()
        .split('/')
        .next()
        .unwrap();

    // Try to use the session with a different repo.
    let url = format!(
        "http://{}/git/session/{}/github.com/other/repo.git/info/refs?service=git-upload-pack",
        server.addr, session_id
    );
    let resp = client.get(&url).send().await.unwrap();
    assert!(
        resp.status().is_client_error(),
        "session repo mismatch should fail, got {}",
        resp.status()
    );
}
