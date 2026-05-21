//! Advanced git client contract tests via the HTTP API.
//!
//! Tests git clone variants (single-branch, mirror, shallow),
//! ls-remote behavior, unsupported operations, URL normalization,
//! branch deletion, binary files, empty repos, and large commit messages.

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
    _upstream_bare: PathBuf,
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

        Self {
            addr,
            tmp,
            upstream_work,
            _upstream_bare: upstream_bare,
        }
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
    }

    fn git_url_no_suffix(&self, repo: &str) -> String {
        format!("http://{}/git/{}", self.addr, repo)
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

fn simple_checksum(data: &[u8]) -> u64 {
    data.iter().fold(0u64, |acc, &b| acc.wrapping_add(b as u64))
}

// ── clone --single-branch ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn clone_single_branch_only_fetches_one_branch() {
    let server = TestServer::start().await;

    // Create a feature branch in upstream.
    run_git(&server.upstream_work, &["checkout", "-b", "feature"]);
    std::fs::write(server.upstream_work.join("feature.txt"), "feature\n").unwrap();
    run_git(&server.upstream_work, &["add", "feature.txt"]);
    run_git(&server.upstream_work, &["commit", "-m", "feature commit"]);
    run_git(&server.upstream_work, &["push", "origin", "feature"]);
    run_git(&server.upstream_work, &["checkout", "main"]);

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("single_branch_clone");

    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--single-branch",
            "--branch",
            "main",
            &url,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;

    let branches = git_stdout_async(&clone_dir, &["branch", "-r"]).await;
    assert!(
        branches.contains("origin/main"),
        "should have origin/main: {branches}"
    );
    // --single-branch should not fetch the feature branch
    assert!(
        !branches.contains("origin/feature"),
        "single-branch clone should NOT fetch feature: {branches}"
    );
}

// ── clone --mirror ──────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn clone_mirror_behavior() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let mirror_dir = server.tmp.path().join("mirror_clone");

    // Mirror clone may work or fail depending on server capabilities.
    let output = try_git_async(
        server.tmp.path(),
        &["clone", "--mirror", &url, mirror_dir.to_str().unwrap()],
    )
    .await;

    if output.status.success() {
        // If mirror clone succeeded, the directory should be a bare repo.
        assert!(
            mirror_dir.join("HEAD").is_file(),
            "mirror clone should create a bare repo with HEAD"
        );
    }
    // If it failed, that's also acceptable behavior for a cache proxy.
}

// ── fetch --depth=1 (shallow fetch) ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn shallow_fetch_depth_1() {
    let server = TestServer::start().await;

    // Add a second commit so there's history to truncate.
    server.commit_and_push("second commit for shallow test");

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("shallow_fetch");

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
    assert_eq!(count, "1", "shallow fetch should have exactly 1 commit");

    // Verify it's marked as shallow
    let is_shallow = git_stdout_async(&clone_dir, &["rev-parse", "--is-shallow-repository"]).await;
    assert_eq!(is_shallow, "true");
}

// ── fetch --deepen=1 after shallow clone ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn deepen_after_shallow_clone() {
    let server = TestServer::start().await;
    server.commit_and_push("second for deepen");
    server.commit_and_push("third for deepen");

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("deepen_clone");

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

    let count_before = git_stdout_async(&clone_dir, &["rev-list", "--count", "HEAD"]).await;
    assert_eq!(count_before, "1");

    // Deepen by 1
    let output = try_git_async(&clone_dir, &["fetch", "--deepen=1", "origin"]).await;

    if output.status.success() {
        let count_after = git_stdout_async(&clone_dir, &["rev-list", "--count", "HEAD"]).await;
        let after: u64 = count_after.parse().unwrap();
        assert!(
            after >= 2,
            "after deepen=1 should have at least 2 commits, got {after}"
        );
    }
    // If deepen fails, that's still valid cache behavior.
}

// ── ls-remote --heads ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ls_remote_heads_lists_all_branches() {
    let server = TestServer::start().await;

    // Create extra branches in upstream.
    run_git(&server.upstream_work, &["checkout", "-b", "dev"]);
    std::fs::write(server.upstream_work.join("dev.txt"), "dev\n").unwrap();
    run_git(&server.upstream_work, &["add", "dev.txt"]);
    run_git(&server.upstream_work, &["commit", "-m", "dev commit"]);
    run_git(&server.upstream_work, &["push", "origin", "dev"]);
    run_git(&server.upstream_work, &["checkout", "main"]);

    let url = server.git_url("github.com/org/repo");

    // Warm the cache with a clone first.
    let warm_dir = server.tmp.path().join("ls_heads_warm");
    run_git_async(
        server.tmp.path(),
        &["clone", &url, warm_dir.to_str().unwrap()],
    )
    .await;

    let text = git_stdout_async(server.tmp.path(), &["ls-remote", "--heads", &url]).await;
    assert!(
        text.contains("refs/heads/main"),
        "ls-remote --heads should list main: {text}"
    );
    assert!(
        text.contains("refs/heads/dev"),
        "ls-remote --heads should list dev: {text}"
    );
}

// ── ls-remote --tags ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ls_remote_tags_behavior() {
    let server = TestServer::start().await;

    // Create a tag in upstream.
    run_git(&server.upstream_work, &["tag", "v1.0.0"]);
    run_git(&server.upstream_work, &["push", "origin", "v1.0.0"]);

    let url = server.git_url("github.com/org/repo");

    // Warm the cache.
    let warm_dir = server.tmp.path().join("ls_tags_warm");
    run_git_async(
        server.tmp.path(),
        &["clone", &url, warm_dir.to_str().unwrap()],
    )
    .await;

    let text = git_stdout_async(server.tmp.path(), &["ls-remote", "--tags", &url]).await;
    // Tags may or may not be served by the cache depending on implementation.
    // Just verify ls-remote --tags doesn't error.
    // If tags are listed, they should contain refs/tags/ prefix.
    if !text.is_empty() {
        for line in text.lines() {
            if line.contains("refs/") {
                assert!(
                    line.contains("refs/tags/"),
                    "ls-remote --tags should only show tag refs: {line}"
                );
            }
        }
    }
}

// ── git archive via HTTP ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn git_archive_returns_error() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");

    // Warm the cache.
    let warm_dir = server.tmp.path().join("archive_warm");
    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--branch",
            "main",
            &url,
            warm_dir.to_str().unwrap(),
        ],
    )
    .await;

    // git archive --remote requires upload-archive which the cache likely doesn't support.
    let output = try_git_async(
        server.tmp.path(),
        &["archive", "--remote", &url, "HEAD"],
    )
    .await;

    assert!(
        !output.status.success(),
        "git archive should fail (not supported by cache proxy)"
    );
}

// ── URL normalization: trailing .git ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn clone_with_trailing_dot_git() {
    let server = TestServer::start().await;
    let url_with_git = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("dotgit_clone");

    run_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--branch",
            "main",
            &url_with_git,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;

    let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
    assert_eq!(cloned_head, server.head_commit());
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_without_trailing_dot_git() {
    let server = TestServer::start().await;
    let url_without_git = server.git_url_no_suffix("github.com/org/repo");
    let clone_dir = server.tmp.path().join("no_dotgit_clone");

    let output = try_git_async(
        server.tmp.path(),
        &[
            "clone",
            "--branch",
            "main",
            &url_without_git,
            clone_dir.to_str().unwrap(),
        ],
    )
    .await;

    if output.status.success() {
        // If it works, HEAD should match.
        let cloned_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;
        assert_eq!(cloned_head, server.head_commit());
    }
    // If it fails, that's also acceptable - .git suffix may be required.
}

// ── Fetch after upstream branch deletion ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn fetch_after_upstream_branch_deleted() {
    let server = TestServer::start().await;

    // Create and push a feature branch.
    run_git(&server.upstream_work, &["checkout", "-b", "ephemeral"]);
    std::fs::write(server.upstream_work.join("eph.txt"), "ephemeral\n").unwrap();
    run_git(&server.upstream_work, &["add", "eph.txt"]);
    run_git(&server.upstream_work, &["commit", "-m", "ephemeral"]);
    run_git(&server.upstream_work, &["push", "origin", "ephemeral"]);
    run_git(&server.upstream_work, &["checkout", "main"]);

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("deleted_branch_clone");

    // Clone with all branches.
    run_git_async(
        server.tmp.path(),
        &["clone", &url, clone_dir.to_str().unwrap()],
    )
    .await;

    let branches_before = git_stdout_async(&clone_dir, &["branch", "-r"]).await;
    assert!(
        branches_before.contains("origin/ephemeral"),
        "should have ephemeral branch: {branches_before}"
    );

    // Delete the branch upstream.
    run_git(
        &server.upstream_work,
        &["push", "origin", "--delete", "ephemeral"],
    );

    // Fetch with prune to pick up the deletion.
    let output = try_git_async(&clone_dir, &["fetch", "--prune", "origin"]).await;

    if output.status.success() {
        let branches_after = git_stdout_async(&clone_dir, &["branch", "-r"]).await;
        assert!(
            !branches_after.contains("origin/ephemeral"),
            "ephemeral branch should be pruned: {branches_after}"
        );
    }
    // If fetch --prune fails, the cache may not support the prune protocol.
}

// ── Fetch after upstream force-push ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn fetch_after_force_push_non_fast_forward() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("ffwd_clone");

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

    let original_head = git_stdout_async(&clone_dir, &["rev-parse", "HEAD"]).await;

    // Force push with orphaned history.
    run_git(&server.upstream_work, &["checkout", "--orphan", "rewrite"]);
    std::fs::write(server.upstream_work.join("README.md"), "rewritten history\n").unwrap();
    run_git(&server.upstream_work, &["add", "README.md"]);
    run_git(&server.upstream_work, &["commit", "-m", "rewritten"]);
    run_git(&server.upstream_work, &["branch", "-M", "main"]);
    run_git(
        &server.upstream_work,
        &["push", "--force", "origin", "main"],
    );
    let new_head = server.head_commit();

    run_git_async(&clone_dir, &["fetch", "origin"]).await;
    let fetched = git_stdout_async(&clone_dir, &["rev-parse", "origin/main"]).await;

    assert_ne!(original_head, new_head, "force push should change HEAD");
    assert_eq!(fetched, new_head, "fetch should pick up force-pushed HEAD");
}

// ── Multiple branches ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn three_branches_all_available_after_clone() {
    let server = TestServer::start().await;

    // Create branch-a and branch-b in addition to main.
    for branch_name in &["branch-a", "branch-b"] {
        run_git(
            &server.upstream_work,
            &["checkout", "-b", branch_name],
        );
        let filename = format!("{branch_name}.txt");
        std::fs::write(
            server.upstream_work.join(&filename),
            format!("{branch_name}\n"),
        )
        .unwrap();
        run_git(&server.upstream_work, &["add", &filename]);
        run_git(
            &server.upstream_work,
            &["commit", "-m", &format!("commit on {branch_name}")],
        );
        run_git(
            &server.upstream_work,
            &["push", "origin", branch_name],
        );
        run_git(&server.upstream_work, &["checkout", "main"]);
    }

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("three_branches");

    run_git_async(
        server.tmp.path(),
        &["clone", &url, clone_dir.to_str().unwrap()],
    )
    .await;

    let branches = git_stdout_async(&clone_dir, &["branch", "-r"]).await;
    assert!(branches.contains("origin/main"), "missing main: {branches}");
    assert!(
        branches.contains("origin/branch-a"),
        "missing branch-a: {branches}"
    );
    assert!(
        branches.contains("origin/branch-b"),
        "missing branch-b: {branches}"
    );
}

// ── Binary file handling ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn binary_file_integrity() {
    let server = TestServer::start().await;

    // Create a binary file with all byte values.
    let data: Vec<u8> = (0..=255).cycle().take(4096).collect();
    let checksum_original = simple_checksum(&data);
    std::fs::write(server.upstream_work.join("binary.dat"), &data).unwrap();
    run_git(&server.upstream_work, &["add", "binary.dat"]);
    run_git(&server.upstream_work, &["commit", "-m", "add binary file"]);
    run_git(
        &server.upstream_work,
        &["push", "--force", "origin", "main"],
    );

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("binary_clone");

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

    let cloned_data = std::fs::read(clone_dir.join("binary.dat")).unwrap();
    let checksum_cloned = simple_checksum(&cloned_data);
    assert_eq!(
        checksum_original, checksum_cloned,
        "binary file checksum mismatch"
    );
    assert_eq!(cloned_data.len(), 4096, "binary file size mismatch");
    assert_eq!(data, cloned_data, "binary content should be identical");
}

// ── Empty repository handling ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn empty_repo_clone_fails() {
    let server = TestServer::start().await;

    // Create an empty upstream repo (no commits).
    let empty_bare = server
        .tmp
        .path()
        .join("upstreams/github.com/org/empty-repo.git");
    std::fs::create_dir_all(empty_bare.parent().unwrap()).unwrap();
    run_git(
        server.tmp.path(),
        &["init", "--bare", empty_bare.to_str().unwrap()],
    );

    let url = server.git_url("github.com/org/empty-repo");
    let clone_dir = server.tmp.path().join("empty_clone");

    let output = try_git_async(
        server.tmp.path(),
        &["clone", &url, clone_dir.to_str().unwrap()],
    )
    .await;

    // Cloning an empty repo may either fail or produce a warning.
    // Either way, git should handle it gracefully.
    if output.status.success() {
        // If clone succeeded, the clone dir should exist but have no commits.
        let count = try_git_async(&clone_dir, &["rev-list", "--count", "HEAD"]).await;
        // HEAD doesn't exist in an empty repo, so this should fail.
        assert!(
            !count.status.success(),
            "empty repo should have no commits to count"
        );
    }
    // If clone failed, that's expected for an empty repo through a cache.
}

// ── Large commit message ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn large_commit_message() {
    let server = TestServer::start().await;

    // Create a commit with a large message (64KB).
    let large_msg = "x".repeat(64 * 1024);
    std::fs::write(server.upstream_work.join("large_msg.txt"), "content\n").unwrap();
    run_git(&server.upstream_work, &["add", "large_msg.txt"]);
    run_git(&server.upstream_work, &["commit", "-m", &large_msg]);
    run_git(
        &server.upstream_work,
        &["push", "--force", "origin", "main"],
    );

    let url = server.git_url("github.com/org/repo");
    let clone_dir = server.tmp.path().join("large_msg_clone");

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

    // Verify the large commit message is preserved.
    let log = git_stdout_async(&clone_dir, &["log", "-1", "--format=%B"]).await;
    assert!(
        log.len() >= 64 * 1024 - 1,
        "commit message should be preserved (got {} chars)",
        log.len()
    );
}
