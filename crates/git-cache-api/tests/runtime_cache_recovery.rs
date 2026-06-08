//! Runtime cache recovery integration tests.
//!
//! These tests exercise the API against a local upstream, local object store,
//! and local hot cache. They focus on cold-cache hydration and partially
//! corrupted hot-cache repos, where durable generation manifests and bundles
//! must repair local state before direct Git fetches are served.

mod support;

use git_cache_api::app;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::net::TcpListener;

const REPO: &str = "github.com/org/repo";

struct TestServer {
    addr: SocketAddr,
    tmp: TempDir,
    upstream_work: PathBuf,
}

impl TestServer {
    async fn start() -> Self {
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

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = support::test_config(addr, tmp.path());

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

    fn git_url(&self) -> String {
        format!("http://{}/git/{REPO}.git", self.addr)
    }

    fn cache_repo_dir(&self) -> PathBuf {
        self.tmp.path().join("cache/repos/github.com/org/repo.git")
    }

    fn object_store_root(&self) -> PathBuf {
        self.tmp.path().join("objects-v2")
    }

    fn head_commit(&self) -> String {
        git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
    }

    fn commit_and_push(&self, contents: &str) -> String {
        fs::write(
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

#[derive(Debug, serde::Deserialize)]
struct MaterializeResponse {
    commit: String,
    source: String,
}

#[derive(Debug, Clone)]
struct CommitManifest {
    generation: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_commit_hydrates_incremental_chain_after_hot_cache_deletion() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let first = server.head_commit();
    let first_manifest = materialize_branch_and_wait(&server, &client, &first).await;

    let second = server.commit_and_push("second");
    let second_manifest = materialize_branch_and_wait(&server, &client, &second).await;
    assert_generation_parent(
        &server,
        &second_manifest.generation,
        Some(&first_manifest.generation),
    );

    let third = server.commit_and_push("third");
    let third_manifest = materialize_branch_and_wait(&server, &client, &third).await;
    assert_generation_parent(
        &server,
        &third_manifest.generation,
        Some(&second_manifest.generation),
    );

    fs::remove_dir_all(server.cache_repo_dir()).unwrap();

    let mat = materialize_exact(&server, &client, &third).await;
    assert_eq!(mat.source, "cache_verified");
    assert_eq!(mat.commit, third);
    assert_eq!(fetch_direct_remote_head(&server, &mat.commit).await, third);

    for commit in [&first, &second, &third] {
        run_git(
            &server.cache_repo_dir(),
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        );
    }
    run_git(&server.cache_repo_dir(), &["fsck", "--connectivity-only"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_commit_repairs_partial_hot_cache_before_direct_fetch() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();

    let commit = server.head_commit();
    let _manifest = materialize_branch_and_wait(&server, &client, &commit).await;

    replace_hot_cache_with_commit_object_only(&server, &commit);
    run_git(
        &server.cache_repo_dir(),
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
    );
    assert_git_fails(
        &server.cache_repo_dir(),
        &["cat-file", "-e", &format!("{commit}^{{tree}}")],
    );

    let mat = materialize_exact(&server, &client, &commit).await;
    assert_eq!(mat.source, "cache_verified");
    assert_eq!(fetch_direct_remote_head(&server, &mat.commit).await, commit);
    run_git(
        &server.cache_repo_dir(),
        &["cat-file", "-e", &format!("{commit}^{{tree}}")],
    );
    run_git(&server.cache_repo_dir(), &["fsck", "--connectivity-only"]);
}

async fn materialize_branch_and_wait(
    server: &TestServer,
    client: &reqwest::Client,
    expected_commit: &str,
) -> CommitManifest {
    let mat = materialize_json(
        server,
        client,
        serde_json::json!({
            "repo": REPO,
            "selector": {"branch": "main"}
        }),
    )
    .await;
    assert_eq!(mat.commit, expected_commit);
    wait_for_commit_manifest(server, expected_commit).await
}

async fn materialize_exact(
    server: &TestServer,
    client: &reqwest::Client,
    commit: &str,
) -> MaterializeResponse {
    materialize_json(
        server,
        client,
        serde_json::json!({
            "repo": REPO,
            "selector": {"commit": commit}
        }),
    )
    .await
}

async fn materialize_json(
    server: &TestServer,
    client: &reqwest::Client,
    body: Value,
) -> MaterializeResponse {
    let response = client
        .post(server.materialize_url())
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        reqwest::StatusCode::OK,
        "materialize response body: {}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.unwrap()
}

async fn wait_for_commit_manifest(server: &TestServer, commit: &str) -> CommitManifest {
    let deadline = Instant::now() + Duration::from_secs(10);
    let path = commit_manifest_path(server, commit);
    loop {
        if let Ok(raw) = fs::read_to_string(&path) {
            let json: Value = serde_json::from_str(&raw).unwrap();
            let generation = json["generation"].as_str().unwrap().to_string();
            let verified = server.object_store_root().join(format!(
                "repos/{REPO}/generations/{generation}/verified.json"
            ));
            let generation_head = server
                .object_store_root()
                .join(format!("repos/{REPO}/manifests/generation-head.json"));
            let head_matches = fs::read_to_string(&generation_head)
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|json| json["generation"].as_str().map(str::to_string))
                .as_deref()
                == Some(generation.as_str());
            if verified.exists() && head_matches {
                return CommitManifest { generation };
            }
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for commit manifest {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn commit_manifest_path(server: &TestServer, commit: &str) -> PathBuf {
    server.object_store_root().join(format!(
        "repos/{REPO}/manifests/commits/{}/{}.json",
        &commit[..2],
        commit
    ))
}

fn assert_generation_parent(server: &TestServer, generation: &str, expected_parent: Option<&str>) {
    let path = server.object_store_root().join(format!(
        "repos/{REPO}/generations/{generation}/manifest.json"
    ));
    let raw = fs::read_to_string(&path).unwrap();
    let json: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        json["parent_generation"].as_str(),
        expected_parent,
        "unexpected parent for generation {generation}"
    );
}

async fn fetch_direct_remote_head(server: &TestServer, commit: &str) -> String {
    let fetch_dir = server.tmp.path().join(format!("fetch-{commit}"));
    fs::create_dir_all(&fetch_dir).unwrap();
    run_git_async(&fetch_dir, &["init"]).await;
    run_git_async(&fetch_dir, &["fetch", &server.git_url(), "refs/heads/main"]).await;
    git_stdout_async(&fetch_dir, &["rev-parse", "FETCH_HEAD"]).await
}

fn replace_hot_cache_with_commit_object_only(server: &TestServer, commit: &str) {
    let repo_dir = server.cache_repo_dir();
    if repo_dir.exists() {
        fs::remove_dir_all(&repo_dir).unwrap();
    }
    fs::create_dir_all(repo_dir.parent().unwrap()).unwrap();
    run_git(
        repo_dir.parent().unwrap(),
        &["init", "--bare", repo_dir.to_str().unwrap()],
    );

    let commit_bytes = git_output(&server.upstream_work, &["cat-file", "commit", commit]);
    let written = run_git_with_stdin(
        &repo_dir,
        &["hash-object", "-w", "-t", "commit", "--stdin"],
        &commit_bytes,
    );
    assert_eq!(String::from_utf8(written).unwrap().trim(), commit);
}

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}\nstdout: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
}

fn assert_git_fails(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "git {:?} unexpectedly succeeded in {}",
        args,
        cwd.display()
    );
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    String::from_utf8(git_output(cwd, args))
        .unwrap()
        .trim()
        .to_string()
}

fn git_output(cwd: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn run_git_with_stdin(cwd: &Path, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut child = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn run_git_async(cwd: &Path, args: &[&str]) {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
        run_git(&cwd, &args_ref);
    })
    .await
    .unwrap();
}

async fn git_stdout_async(cwd: &Path, args: &[&str]) -> String {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let args_ref = args.iter().map(String::as_str).collect::<Vec<_>>();
        git_stdout(&cwd, &args_ref)
    })
    .await
    .unwrap()
}
