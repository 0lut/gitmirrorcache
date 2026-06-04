//! Advanced performance tests for the Git wrapper.

use git_cache_core::CommitSha;
use git_cache_git::Git;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    path: PathBuf,
}

impl TempTree {
    fn new(name: &str) -> Self {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "git-cache-perf-adv-{name}-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp tree");
        Self { path }
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn test_git() -> Git {
    Git::default_with_timeout(Duration::from_secs(30))
}

fn run_git<I, S>(cwd: Option<&Path>, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let mut command = Command::new("git");
    command
        .args(&args)
        .env_clear()
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ASKPASS", "/bin/false")
        .env("SSH_ASKPASS", "/bin/false")
        .env("HOME", "/nonexistent");

    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }

    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let output = command.output().expect("run setup git command");
    assert!(
        output.status.success(),
        "git {:?} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        args,
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("test paths are utf-8")
}

fn create_source_repo(root: &Path) -> (PathBuf, String) {
    let source_repo = root.join("source");
    run_git(None, ["init", "--", path_arg(&source_repo)]);
    run_git(Some(&source_repo), ["checkout", "-B", "main"]);
    run_git(
        Some(&source_repo),
        ["config", "user.email", "test@example.invalid"],
    );
    run_git(
        Some(&source_repo),
        ["config", "user.name", "Git Cache Test"],
    );
    run_git(Some(&source_repo), ["config", "gc.auto", "0"]);
    run_git(Some(&source_repo), ["config", "maintenance.auto", "false"]);

    std::fs::write(source_repo.join("README.md"), "hello from git-cache\n").expect("write README");
    run_git(Some(&source_repo), ["add", "README.md"]);
    run_git(Some(&source_repo), ["commit", "-m", "initial"]);

    let sha = run_git(Some(&source_repo), ["rev-parse", "HEAD"]);
    (source_repo, sha)
}

fn create_source_repo_with_commits(root: &Path, commit_count: usize) -> (PathBuf, String) {
    let source_repo = root.join("source");
    run_git(None, ["init", "--", path_arg(&source_repo)]);
    run_git(Some(&source_repo), ["checkout", "-B", "main"]);
    run_git(
        Some(&source_repo),
        ["config", "user.email", "test@example.invalid"],
    );
    run_git(
        Some(&source_repo),
        ["config", "user.name", "Git Cache Test"],
    );
    run_git(Some(&source_repo), ["config", "gc.auto", "0"]);
    run_git(Some(&source_repo), ["config", "maintenance.auto", "false"]);

    for i in 0..commit_count {
        std::fs::write(source_repo.join("README.md"), format!("commit {i}\n"))
            .expect("write README");
        run_git(Some(&source_repo), ["add", "README.md"]);
        run_git(Some(&source_repo), ["commit", "-m", &format!("commit {i}")]);
    }

    let sha = run_git(Some(&source_repo), ["rev-parse", "HEAD"]);
    (source_repo, sha)
}

fn create_source_repo_with_branches(root: &Path, branch_count: usize) -> PathBuf {
    let source_repo = root.join("source");
    run_git(None, ["init", "--", path_arg(&source_repo)]);
    run_git(Some(&source_repo), ["checkout", "-B", "main"]);
    run_git(
        Some(&source_repo),
        ["config", "user.email", "test@example.invalid"],
    );
    run_git(
        Some(&source_repo),
        ["config", "user.name", "Git Cache Test"],
    );
    run_git(Some(&source_repo), ["config", "gc.auto", "0"]);
    run_git(Some(&source_repo), ["config", "maintenance.auto", "false"]);

    std::fs::write(source_repo.join("README.md"), "base\n").expect("write README");
    run_git(Some(&source_repo), ["add", "README.md"]);
    run_git(Some(&source_repo), ["commit", "-m", "initial"]);

    for i in 0..branch_count {
        run_git(
            Some(&source_repo),
            ["branch", &format!("feature-{i}"), "main"],
        );
    }

    source_repo
}

// ── 1. fetch_branch throughput ───────────────────────────────────────────

#[tokio::test]
async fn test_fetch_branch_throughput() {
    let temp = TempTree::new("fetch-branch-throughput");
    let git = test_git();
    let (source_repo, _) = create_source_repo(&temp.path);

    let cache_repo = temp.path.join("cache.git");
    git.init_bare(&cache_repo).await.unwrap();

    let iterations = 20;
    let mut latencies = Vec::with_capacity(iterations);

    let start = Instant::now();
    for _ in 0..iterations {
        let iter_start = Instant::now();
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .unwrap();
        latencies.push(iter_start.elapsed());
    }
    let total = start.elapsed();

    latencies.sort();
    let avg = total / iterations as u32;
    let p50 = latencies[latencies.len() / 2];
    eprintln!(
        "fetch_branch throughput: {iterations} fetches in {total:?}, avg={avg:?}, p50={p50:?}"
    );
    assert!(
        total.as_secs() < 120,
        "fetch_branch throughput too slow: {total:?}"
    );
}

// ── 2. bundle_create scaling ─────────────────────────────────────────────

#[tokio::test]
async fn test_bundle_create_scaling() {
    let commit_counts = [10, 50, 200];
    let mut timings = Vec::new();

    for &count in &commit_counts {
        let temp = TempTree::new(&format!("bundle-scale-{count}"));
        let git = test_git();
        let (source_repo, _) = create_source_repo_with_commits(&temp.path, count);

        let cache_repo = temp.path.join("cache.git");
        git.init_bare(&cache_repo).await.unwrap();
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .unwrap();

        let bundle_path = temp.path.join("scale.bundle");

        let start = Instant::now();
        git.bundle_create(&cache_repo, &bundle_path, "refs/cache/main")
            .await
            .unwrap();
        let elapsed = start.elapsed();

        let bundle_size = std::fs::metadata(&bundle_path).unwrap().len();
        timings.push((count, elapsed, bundle_size));
    }

    eprintln!("bundle_create scaling:");
    for (count, elapsed, size) in &timings {
        eprintln!("  {count} commits: {elapsed:?}, bundle={size} bytes");
    }

    // Each timing should be reasonable.
    for (count, elapsed, _) in &timings {
        assert!(
            elapsed.as_secs() < 120,
            "bundle_create for {count} commits too slow: {elapsed:?}"
        );
    }
}

// ── 3. bundle_create_incremental vs full ─────────────────────────────────

#[tokio::test]
async fn test_bundle_create_incremental_vs_full() {
    let temp = TempTree::new("bundle-incr-vs-full");
    let git = test_git();
    let (source_repo, _) = create_source_repo_with_commits(&temp.path, 100);

    let cache_repo = temp.path.join("cache.git");
    git.init_bare(&cache_repo).await.unwrap();
    git.fetch_branch(
        &cache_repo,
        path_arg(&source_repo),
        "main",
        "refs/cache/main",
    )
    .await
    .unwrap();

    // Full bundle.
    let full_bundle = temp.path.join("full.bundle");
    let start = Instant::now();
    git.bundle_create(&cache_repo, &full_bundle, "refs/cache/main")
        .await
        .unwrap();
    let full_elapsed = start.elapsed();
    let full_size = std::fs::metadata(&full_bundle).unwrap().len();

    // Get a mid-point commit for incremental base (exclude tip).
    let base_rev_str = git
        .rev_parse(&cache_repo, "refs/cache/main~50")
        .await
        .unwrap();
    let base_sha = CommitSha::parse(&base_rev_str).unwrap();

    // Incremental bundle (all refs minus the base commit).
    let incr_bundle = temp.path.join("incr.bundle");
    let start = Instant::now();
    git.bundle_create_incremental(&cache_repo, &incr_bundle, &[base_sha])
        .await
        .unwrap();
    let incr_elapsed = start.elapsed();
    let incr_size = std::fs::metadata(&incr_bundle).unwrap().len();

    eprintln!(
        "bundle full vs incremental (100 commits):\n  full:  {full_elapsed:?}, {full_size} bytes\n  incr:  {incr_elapsed:?}, {incr_size} bytes"
    );

    // Incremental bundle should be smaller.
    assert!(
        incr_size < full_size,
        "incremental bundle ({incr_size}) should be smaller than full ({full_size})"
    );
    assert!(
        full_elapsed.as_secs() < 120,
        "full bundle too slow: {full_elapsed:?}"
    );
    assert!(
        incr_elapsed.as_secs() < 120,
        "incremental bundle too slow: {incr_elapsed:?}"
    );
}

// ── 4. rev_parse throughput ──────────────────────────────────────────────

#[tokio::test]
async fn test_rev_parse_throughput_100() {
    let temp = TempTree::new("rev-parse-100");
    let git = test_git();
    let (source_repo, _) = create_source_repo(&temp.path);

    let cache_repo = temp.path.join("cache.git");
    git.init_bare(&cache_repo).await.unwrap();
    git.fetch_branch(
        &cache_repo,
        path_arg(&source_repo),
        "main",
        "refs/cache/main",
    )
    .await
    .unwrap();

    let iterations = 100u32;
    let start = Instant::now();
    for _ in 0..iterations {
        let sha = git
            .rev_parse(&cache_repo, "refs/cache/main^{commit}")
            .await
            .unwrap();
        assert!(!sha.is_empty());
    }
    let elapsed = start.elapsed();

    let avg = elapsed / iterations;
    eprintln!("rev_parse throughput: {iterations} calls in {elapsed:?}, avg={avg:?}");
    assert!(
        elapsed.as_secs() < 120,
        "rev_parse throughput too slow: {elapsed:?}"
    );
}

// ── 5. Concurrent init_bare ──────────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_init_bare() {
    let temp = TempTree::new("concurrent-init-bare");
    let git = test_git();
    let count = 50;

    let start = Instant::now();
    let mut handles = Vec::with_capacity(count);
    for i in 0..count {
        let git = git.clone();
        let repo_dir = temp.path.join(format!("repo-{i}.git"));
        handles.push(tokio::spawn(async move {
            git.init_bare(&repo_dir).await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
    let elapsed = start.elapsed();

    let ops_per_sec = count as f64 / elapsed.as_secs_f64();
    eprintln!("concurrent init_bare: {count} repos in {elapsed:?} ({ops_per_sec:.0} ops/sec)");
    assert!(
        elapsed.as_secs() < 120,
        "concurrent init_bare too slow: {elapsed:?}"
    );
}

// ── 6. upload_pack_advertise_refs scaling ────────────────────────────────

#[tokio::test]
async fn test_upload_pack_advertise_refs_scaling() {
    let branch_counts = [1, 10, 50];
    let mut timings = Vec::new();

    for &branch_count in &branch_counts {
        let temp = TempTree::new(&format!("advertise-scale-{branch_count}"));
        let git = test_git();
        let source_repo = create_source_repo_with_branches(&temp.path, branch_count);

        let cache_repo = temp.path.join("cache.git");
        git.init_bare(&cache_repo).await.unwrap();

        // Fetch main and all feature branches.
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .unwrap();
        for i in 0..branch_count {
            git.fetch_branch(
                &cache_repo,
                path_arg(&source_repo),
                &format!("feature-{i}"),
                &format!("refs/cache/feature-{i}"),
            )
            .await
            .unwrap();
        }

        let start = Instant::now();
        let advertised = git
            .upload_pack_advertise_refs(&cache_repo, 128 * 1024)
            .await
            .unwrap();
        let elapsed = start.elapsed();

        let ref_count = branch_count + 1; // +1 for main
        timings.push((ref_count, elapsed, advertised.stdout.len()));
    }

    eprintln!("upload_pack_advertise_refs scaling:");
    for (ref_count, elapsed, size) in &timings {
        eprintln!("  {ref_count} refs: {elapsed:?}, {size} bytes output");
    }

    for (ref_count, elapsed, _) in &timings {
        assert!(
            elapsed.as_secs() < 30,
            "advertise_refs with {ref_count} refs too slow: {elapsed:?}"
        );
    }
}
