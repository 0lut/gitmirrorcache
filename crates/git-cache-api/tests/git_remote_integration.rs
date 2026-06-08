//! Integration tests for the read-through Git remote.
//!
//! These tests spin up a real Axum server with a local upstream, run actual
//! `git clone` / `git fetch` commands against it, and verify the results.

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
            max_concurrent_generation_verifications: 1,
        };

        let router = app(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let server = Self {
            addr,
            tmp,
            upstream_work,
            upstream_bare,
        };
        server.warm_all_heads();
        server
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
    }

    fn head_commit(&self) -> String {
        git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
    }

    fn commit_and_push(&self, contents: &str) -> String {
        let commit = self.commit_and_push_without_warming(contents);
        self.warm_all_heads();
        commit
    }

    fn commit_and_push_without_warming(&self, contents: &str) -> String {
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

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn cold_direct_ref_advertisement_does_not_require_local_repo() {
    let server = TestServer::start().await;
    let repo_dir = server
        .tmp
        .path()
        .join("cache/repos/github.com/org/repo.git");
    std::fs::remove_dir_all(&repo_dir).unwrap();

    let url = format!(
        "http://{}/git/github.com/org/repo.git/info/refs?service=git-upload-pack",
        server.addr
    );
    let response = reqwest::get(&url).await.unwrap();

    assert_eq!(response.status(), 200);
    let body = response.bytes().await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("refs/heads/main")
            || body
                .windows(b"refs/heads/main".len())
                .any(|w| w == b"refs/heads/main"),
        "cold direct GET should advertise upstream refs"
    );
    assert!(
        !repo_dir.join("config").exists(),
        "cold direct GET must not initialize or fetch the local repo"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_branch_via_direct_remote() {
    let server = TestServer::start().await;
    let clone_dir = server.tmp.path().join("clone1");
    let url = server.git_url("github.com/org/repo");

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
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_clone_does_not_refetch_when_unchanged() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    let clone1 = server.tmp.path().join("repeat1");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone1.to_str().unwrap(),
        ],
    )
    .await;
    let head1 = git_stdout_async(&clone1, &["rev-parse", "HEAD"]).await;

    let clone2 = server.tmp.path().join("repeat2");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone2.to_str().unwrap(),
        ],
    )
    .await;
    let head2 = git_stdout_async(&clone2, &["rev-parse", "HEAD"]).await;

    assert_eq!(head1, head2);
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_picks_up_new_branch_head() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    let clone1 = server.tmp.path().join("adv1");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone1.to_str().unwrap(),
        ],
    )
    .await;
    let first_head = git_stdout_async(&clone1, &["rev-parse", "HEAD"]).await;

    let new_commit = server.commit_and_push("second commit");

    let clone2 = server.tmp.path().join("adv2");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone2.to_str().unwrap(),
        ],
    )
    .await;
    let second_head = git_stdout_async(&clone2, &["rev-parse", "HEAD"]).await;

    assert_ne!(first_head, second_head);
    assert_eq!(second_head, new_commit);
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_reads_through_when_upstream_advances_without_warming() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let cached_head = server.head_commit();

    let upstream_head = server.commit_and_push_without_warming("unwarmed commit");
    assert_ne!(cached_head, upstream_head);

    let clone = server.tmp.path().join("stale-but-local");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone.to_str().unwrap(),
        ],
    )
    .await;

    let cloned_head = git_stdout_async(&clone, &["rev-parse", "HEAD"]).await;
    assert_eq!(
        cloned_head, upstream_head,
        "direct Git should advertise and read through the current upstream tip"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_exact_commit_sha() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // First clone to warm the cache.
    let clone1 = server.tmp.path().join("sha1");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone1.to_str().unwrap(),
        ],
    )
    .await;
    let head = server.head_commit();

    // Now fetch by exact SHA from a fresh init.
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
async fn filtered_clone_checkout_fetches_blob_wants() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("filtered");

    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--depth=1",
            "--filter=blob:none",
            "--no-checkout",
            &url,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;
    run_git_async(&clone_dir, &["checkout", "-q", "HEAD"]).await;

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());

    let readme = std::fs::read_to_string(clone_dir.join("README.md")).unwrap();
    assert_eq!(readme.trim(), "initial");
}

#[tokio::test(flavor = "multi_thread")]
async fn receive_pack_rejected_on_direct_remote() {
    let server = TestServer::start().await;
    let url = format!(
        "http://{}/git/github.com/org/repo.git/info/refs?service=git-receive-pack",
        server.addr
    );

    let response = reqwest::get(&url).await.unwrap();
    assert_eq!(response.status(), 405);
}

#[tokio::test(flavor = "multi_thread")]
async fn internal_cache_refs_are_hidden() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // Clone to populate the cache.
    let clone1 = server.tmp.path().join("hide1");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--no-tags",
            "--branch",
            "main",
            &url,
            clone1.to_str().unwrap(),
        ],
    )
    .await;

    // ls-remote should not show refs/cache/*
    let text = git_stdout_async(server.tmp.path(), &["ls-remote", &url]).await;

    for line in text.lines() {
        assert!(!line.contains("refs/cache"), "internal ref leaked: {line}");
    }
}
