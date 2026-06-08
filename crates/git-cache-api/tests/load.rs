//! Load tests simulating large repository behavior at the API layer.
//!
//! Each test creates local upstream repos that mimic large repo features
//! (many commits, many branches, large files, etc.) and exercises the
//! cache API against them.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Barrier;

// ── Helpers ─────────────────────────────────────────────────────────────

struct TestServer {
    addr: std::net::SocketAddr,
    tmp: TempDir,
}

impl TestServer {
    async fn start_with_upstream(upstream_root: &Path) -> Self {
        let tmp = TempDir::new().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let config = AppConfig {
            bind_addr: addr,
            public_base_url: format!("http://{addr}"),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(upstream_root.to_path_buf()),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 300,
            max_git_output_bytes: 128 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            max_concurrent_generation_verifications: 1,
            leases: Default::default(),
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 2 * 1024 * 1024 * 1024,
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

        Self { addr, tmp }
    }

    fn materialize_url(&self) -> String {
        format!("http://{}/v1/materialize", self.addr)
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{repo}.git", self.addr)
    }
}

fn run_git(cwd: &Path, args: &[&str]) {
    for attempt in 0..3 {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        if output.status.success() {
            return;
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        panic!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
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

fn try_git_clone(cwd: &Path, url: &str, dest: &str) -> bool {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(["clone", url, dest])
        .output()
        .unwrap();
    output.status.success()
}

/// Create an upstream bare repo with `n` commits, returning (bare_path, work_dir, head_sha).
fn create_repo_with_n_commits(
    upstream_root: &Path,
    repo_key: &str,
    n: usize,
) -> (PathBuf, PathBuf, String) {
    let bare_path = upstream_root.join(format!("{repo_key}.git"));
    let work_dir = upstream_root.join(format!("{repo_key}-work"));

    std::fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();

    run_git(
        upstream_root,
        &["init", "--bare", bare_path.to_str().unwrap()],
    );
    run_git(&work_dir, &["init"]);
    run_git(&work_dir, &["config", "user.email", "test@example.com"]);
    run_git(&work_dir, &["config", "user.name", "Load Test"]);
    run_git(
        &work_dir,
        &["remote", "add", "origin", bare_path.to_str().unwrap()],
    );

    for i in 0..n {
        std::fs::write(work_dir.join("data.txt"), format!("commit {i}\n")).unwrap();
        run_git(&work_dir, &["add", "data.txt"]);
        run_git(&work_dir, &["commit", "-m", &format!("commit {i}")]);
    }

    run_git(&work_dir, &["branch", "-M", "main"]);
    run_git(&work_dir, &["push", "origin", "main"]);
    run_git(&bare_path, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let head_sha = git_stdout(&work_dir, &["rev-parse", "HEAD"]);
    (bare_path, work_dir, head_sha)
}

/// Create upstream bare repo with `n` branches.
fn create_repo_with_n_branches(
    upstream_root: &Path,
    repo_key: &str,
    n: usize,
) -> (PathBuf, PathBuf) {
    let bare_path = upstream_root.join(format!("{repo_key}.git"));
    let work_dir = upstream_root.join(format!("{repo_key}-work"));

    std::fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();

    run_git(
        upstream_root,
        &["init", "--bare", bare_path.to_str().unwrap()],
    );
    run_git(&work_dir, &["init"]);
    run_git(&work_dir, &["config", "user.email", "test@example.com"]);
    run_git(&work_dir, &["config", "user.name", "Load Test"]);
    run_git(
        &work_dir,
        &["remote", "add", "origin", bare_path.to_str().unwrap()],
    );

    std::fs::write(work_dir.join("base.txt"), "base\n").unwrap();
    run_git(&work_dir, &["add", "base.txt"]);
    run_git(&work_dir, &["commit", "-m", "base"]);
    run_git(&work_dir, &["branch", "-M", "main"]);
    run_git(&work_dir, &["push", "origin", "main"]);

    for i in 0..n {
        let branch_name = format!("feature-{i}");
        run_git(&work_dir, &["checkout", "-B", &branch_name, "main"]);
        std::fs::write(work_dir.join("branch.txt"), format!("branch {i}\n")).unwrap();
        run_git(&work_dir, &["add", "branch.txt"]);
        run_git(
            &work_dir,
            &["commit", "-m", &format!("commit on {branch_name}")],
        );
        run_git(&work_dir, &["push", "origin", &branch_name]);
    }

    run_git(&work_dir, &["checkout", "main"]);
    run_git(&bare_path, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    (bare_path, work_dir)
}

// ── 1. Many commits repo ────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn many_commits_repo() {
    let upstream_root = TempDir::new().unwrap();
    let (_bare, _work, expected_head) =
        create_repo_with_n_commits(upstream_root.path(), "github.com/org/many-commits", 100);

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    let start = Instant::now();
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/many-commits",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(resp.status().as_u16(), 200, "materialize should succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    let commit = body["commit"].as_str().unwrap().to_string();
    let git_url = server.git_url("github.com/org/many-commits");

    assert_eq!(
        commit, expected_head,
        "materialized commit should match HEAD"
    );

    // Clone through the cache's direct Git remote.
    let clone_dir = server.tmp.path().join("many-commits-clone");
    run_git_async(
        server.tmp.path(),
        &["clone", &git_url, clone_dir.to_str().unwrap()],
    )
    .await;

    let clone_head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
    assert_eq!(
        clone_head, expected_head,
        "cloned HEAD should match upstream"
    );

    eprintln!("many_commits_repo: materialize took {elapsed:?}");
}

// ── 2. Many branches repo ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn many_branches_repo() {
    let upstream_root = TempDir::new().unwrap();
    let branch_count = 100;
    create_repo_with_n_branches(
        upstream_root.path(),
        "github.com/org/many-branches",
        branch_count,
    );

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    // Materialize each branch to verify the cache handles many branches
    let mut successes = 0;
    for i in 0..branch_count {
        let branch = format!("feature-{i}");
        let resp = client
            .post(server.materialize_url())
            .json(&serde_json::json!({
                "repo": "github.com/org/many-branches",
                "selector": {"branch": branch}
            }))
            .send()
            .await
            .unwrap();

        if resp.status().as_u16() == 200 {
            successes += 1;
        }
    }

    assert!(
        successes >= branch_count / 2,
        "at least half of branch materializations should succeed, got {successes}/{branch_count}"
    );

    // Verify main branch materializes and is advertised by the direct remote.
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/many-branches",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let git_url = server.git_url("github.com/org/many-branches");
    let ref_name = "refs/heads/main";

    // Verify the branch ref is advertised via info/refs.
    let refs_url = format!("{git_url}/info/refs?service=git-upload-pack");
    let refs_resp = client.get(&refs_url).send().await.unwrap();
    assert_eq!(refs_resp.status().as_u16(), 200);
    let refs_body = refs_resp.text().await.unwrap();
    assert!(
        refs_body.contains(ref_name),
        "branch ref {ref_name} should be in ref advertisement"
    );
}

// ── 3. Large files repo ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn large_files_repo() {
    let upstream_root = TempDir::new().unwrap();
    let bare_path = upstream_root.path().join("github.com/org/large-files.git");
    let work_dir = upstream_root.path().join("github.com/org/large-files-work");

    std::fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();

    run_git(
        upstream_root.path(),
        &["init", "--bare", bare_path.to_str().unwrap()],
    );
    run_git(&work_dir, &["init"]);
    run_git(&work_dir, &["config", "user.email", "test@example.com"]);
    run_git(&work_dir, &["config", "user.name", "Load Test"]);
    run_git(
        &work_dir,
        &["remote", "add", "origin", bare_path.to_str().unwrap()],
    );

    // Create a 10MB binary file
    let large_data: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
    let expected_sha256 = sha256_hex(&large_data);
    std::fs::write(work_dir.join("large.bin"), &large_data).unwrap();

    run_git(&work_dir, &["add", "large.bin"]);
    run_git(&work_dir, &["commit", "-m", "add large binary"]);
    run_git(&work_dir, &["branch", "-M", "main"]);
    run_git(&work_dir, &["push", "origin", "main"]);
    run_git(&bare_path, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/large-files",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let git_url = server.git_url("github.com/org/large-files");

    let clone_dir = server.tmp.path().join("large-files-clone");
    run_git_async(
        server.tmp.path(),
        &["clone", &git_url, clone_dir.to_str().unwrap()],
    )
    .await;

    let cloned_data = std::fs::read(clone_dir.join("large.bin")).unwrap();
    let cloned_sha256 = sha256_hex(&cloned_data);
    assert_eq!(
        cloned_sha256, expected_sha256,
        "SHA256 of cloned large file should match original"
    );
}

fn sha256_hex(data: &[u8]) -> String {
    use std::io::Write;
    let mut child = Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("sha256sum");
    child.stdin.as_mut().unwrap().write_all(data).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

// ── 4. Deep history fetch ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn deep_history_fetch() {
    let upstream_root = TempDir::new().unwrap();
    let (_bare, work_dir, initial_head) =
        create_repo_with_n_commits(upstream_root.path(), "github.com/org/deep-history", 100);

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    // First materialize + clone
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/deep-history",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let commit = body["commit"].as_str().unwrap().to_string();
    let git_url = server.git_url("github.com/org/deep-history");
    assert_eq!(commit, initial_head);

    let clone_dir = server.tmp.path().join("deep-history-clone");
    run_git_async(
        server.tmp.path(),
        &["clone", &git_url, clone_dir.to_str().unwrap()],
    )
    .await;

    // Push 50 more commits upstream
    for i in 0..50 {
        std::fs::write(work_dir.join("data.txt"), format!("extra {i}\n")).unwrap();
        run_git(&work_dir, &["add", "data.txt"]);
        run_git(&work_dir, &["commit", "-m", &format!("extra {i}")]);
    }
    run_git(&work_dir, &["push", "origin", "main"]);
    let new_head = git_stdout(&work_dir, &["rev-parse", "HEAD"]);

    // Re-materialize to get updated cache
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/deep-history",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let commit2 = body["commit"].as_str().unwrap().to_string();
    assert_eq!(
        commit2, new_head,
        "re-materialized commit should be new HEAD"
    );

    // Clone the updated version
    let clone_dir2 = server.tmp.path().join("deep-history-clone2");
    run_git_async(
        server.tmp.path(),
        &["clone", &git_url, clone_dir2.to_str().unwrap()],
    )
    .await;

    // Verify all 150 commits are accessible (100 initial + 50 extra)
    let count = git_stdout(&clone_dir2, &["rev-list", "--count", "HEAD"]);
    assert_eq!(
        count.parse::<usize>().unwrap(),
        150,
        "should have all 150 commits"
    );
}

// ── 5. Rapid update cycle ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn rapid_update_cycle() {
    let upstream_root = TempDir::new().unwrap();
    let bare_path = upstream_root.path().join("github.com/org/rapid-update.git");
    let work_dir = upstream_root
        .path()
        .join("github.com/org/rapid-update-work");

    std::fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();

    run_git(
        upstream_root.path(),
        &["init", "--bare", bare_path.to_str().unwrap()],
    );
    run_git(&work_dir, &["init"]);
    run_git(&work_dir, &["config", "user.email", "test@example.com"]);
    run_git(&work_dir, &["config", "user.name", "Load Test"]);
    run_git(
        &work_dir,
        &["remote", "add", "origin", bare_path.to_str().unwrap()],
    );
    std::fs::write(work_dir.join("data.txt"), "initial\n").unwrap();
    run_git(&work_dir, &["add", "data.txt"]);
    run_git(&work_dir, &["commit", "-m", "initial"]);
    run_git(&work_dir, &["branch", "-M", "main"]);
    run_git(&work_dir, &["push", "origin", "main"]);
    run_git(&bare_path, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    for cycle in 0..20 {
        // Push a new commit
        std::fs::write(work_dir.join("data.txt"), format!("cycle {cycle}\n")).unwrap();
        run_git(&work_dir, &["add", "data.txt"]);
        run_git(&work_dir, &["commit", "-m", &format!("cycle {cycle}")]);
        run_git(&work_dir, &["push", "origin", "main"]);

        let expected_head = git_stdout(&work_dir, &["rev-parse", "HEAD"]);

        // Materialize
        let resp = client
            .post(server.materialize_url())
            .json(&serde_json::json!({
                "repo": "github.com/org/rapid-update",
                "selector": {"branch": "main"}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            200,
            "materialize cycle {cycle} should succeed"
        );

        let body: serde_json::Value = resp.json().await.unwrap();
        let commit = body["commit"].as_str().unwrap_or("").to_string();
        assert_eq!(
            commit, expected_head,
            "cycle {cycle}: materialized commit should match latest push"
        );
    }
}

// ── 6. Concurrent large clones ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_large_clones() {
    let upstream_root = TempDir::new().unwrap();
    let (_bare, _work, expected_head) =
        create_repo_with_n_commits(upstream_root.path(), "github.com/org/concurrent-large", 100);

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    // Materialize to populate cache before direct Git clones.
    let resp = client
        .post(server.materialize_url())
        .json(&serde_json::json!({
            "repo": "github.com/org/concurrent-large",
            "selector": {"branch": "main"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let commit = body["commit"].as_str().unwrap().to_string();
    assert_eq!(commit, expected_head);

    let git_url = server.git_url("github.com/org/concurrent-large");

    // Concurrent clones share the normal direct Git remote URL.
    let barrier = Arc::new(Barrier::new(10));
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let bar = Arc::clone(&barrier);
            let clone_base = server.tmp.path().to_path_buf();
            let clone_url = git_url.clone();
            tokio::spawn(async move {
                bar.wait().await;
                let clone_dir = clone_base.join(format!("conc-clone-{i}"));
                let dest = clone_dir.to_str().unwrap().to_string();
                let cwd = clone_base.clone();
                let ok =
                    tokio::task::spawn_blocking(move || try_git_clone(&cwd, &clone_url, &dest))
                        .await
                        .unwrap();
                if ok {
                    let cd = clone_dir.clone();
                    let head = tokio::task::spawn_blocking(move || {
                        git_stdout(&cd, &["rev-parse", "HEAD"])
                    })
                    .await
                    .unwrap();
                    Some(head)
                } else {
                    None
                }
            })
        })
        .collect();

    let mut successes = 0;
    for handle in handles {
        if let Some(head) = handle.await.unwrap() {
            assert_eq!(head, expected_head, "cloned HEAD should match");
            successes += 1;
        }
    }

    assert!(
        successes >= 1,
        "at least one concurrent clone should succeed"
    );
}

// ── 7. Multi-repo load ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn multi_repo_load() {
    let upstream_root = TempDir::new().unwrap();

    let mut expected_heads = Vec::new();
    for i in 0..5 {
        let (_, _, head) = create_repo_with_n_commits(
            upstream_root.path(),
            &format!("github.com/org/multi-{i}"),
            10,
        );
        expected_heads.push(head);
    }

    let server = TestServer::start_with_upstream(upstream_root.path()).await;
    let client = reqwest::Client::new();

    // 3 rounds of materializing all 5 repos in parallel
    for round in 0..3 {
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let c = client.clone();
                let url = server.materialize_url();
                let expected = expected_heads[i].clone();
                tokio::spawn(async move {
                    let resp = c
                        .post(&url)
                        .json(&serde_json::json!({
                            "repo": format!("github.com/org/multi-{i}"),
                            "selector": {"branch": "main"}
                        }))
                        .send()
                        .await
                        .unwrap();
                    let status = resp.status().as_u16();
                    if status == 200 {
                        let body: serde_json::Value = resp.json().await.unwrap();
                        let commit = body["commit"].as_str().unwrap_or("").to_string();
                        (i, status, Some(commit), expected)
                    } else {
                        (i, status, None, expected)
                    }
                })
            })
            .collect();

        for handle in handles {
            let (i, status, commit, expected) = handle.await.unwrap();
            if status == 200 {
                assert_eq!(
                    commit.as_deref(),
                    Some(expected.as_str()),
                    "round {round}, repo {i}: commit mismatch"
                );
            }
        }
    }
}
