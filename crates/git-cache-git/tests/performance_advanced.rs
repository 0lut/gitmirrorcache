//! Advanced performance tests for the Git wrapper.

mod common;

mod tests {
    use git_cache_core::CommitSha;
    use git_cache_git::{FetchOptions, Git};
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
        command.args(&args);
        crate::common::configure_git_env(&mut command);

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

        std::fs::write(source_repo.join("README.md"), "hello from git-cache\n")
            .expect("write README");
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

        for i in 0..commit_count {
            std::fs::write(source_repo.join("README.md"), format!("commit {i}\n"))
                .expect("write README");
            run_git(Some(&source_repo), ["add", "README.md"]);
            run_git(Some(&source_repo), ["commit", "-m", &format!("commit {i}")]);
        }

        let sha = run_git(Some(&source_repo), ["rev-parse", "HEAD"]);
        (source_repo, sha)
    }

    // ── 1. fetch_ref throughput ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_fetch_ref_throughput() {
        let temp = TempTree::new("fetch-ref-throughput");
        let git = test_git();
        let (source_repo, _) = create_source_repo(&temp.path);

        let cache_repo = temp.path.join("cache.git");
        git.init_bare(&cache_repo).await.unwrap();

        let iterations = 20;
        let mut latencies = Vec::with_capacity(iterations);

        let start = Instant::now();
        for _ in 0..iterations {
            let iter_start = Instant::now();
            git.fetch_ref(
                &cache_repo,
                path_arg(&source_repo),
                "refs/heads/main",
                "refs/cache/main",
                FetchOptions::default(),
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
            "fetch_ref throughput: {iterations} fetches in {total:?}, avg={avg:?}, p50={p50:?}"
        );
        assert!(
            total.as_secs() < 120,
            "fetch_ref throughput too slow: {total:?}"
        );
    }

    // ── 2. pack_objects_revs scaling ─────────────────────────────────────────

    #[tokio::test]
    async fn test_pack_objects_scaling() {
        let commit_counts = [10, 50, 200];
        let mut timings = Vec::new();

        for &count in &commit_counts {
            let temp = TempTree::new(&format!("pack-scale-{count}"));
            let git = test_git();
            let (source_repo, source_sha) = create_source_repo_with_commits(&temp.path, count);

            let cache_repo = temp.path.join("cache.git");
            git.init_bare(&cache_repo).await.unwrap();
            git.fetch_ref(
                &cache_repo,
                path_arg(&source_repo),
                "refs/heads/main",
                "refs/cache/main",
                FetchOptions::default(),
            )
            .await
            .unwrap();

            let head = CommitSha::parse(&source_sha).unwrap();
            let start = Instant::now();
            let pack_path = git
                .pack_objects_revs(
                    &cache_repo,
                    &temp.path.join("scale-pack"),
                    std::slice::from_ref(&head),
                    &[],
                )
                .await
                .unwrap();
            let elapsed = start.elapsed();

            let pack_size = std::fs::metadata(&pack_path).unwrap().len();
            timings.push((count, elapsed, pack_size));
        }

        eprintln!("pack_objects_revs scaling:");
        for (count, elapsed, size) in &timings {
            eprintln!("  {count} commits: {elapsed:?}, pack={size} bytes");
        }

        // Each timing should be reasonable.
        for (count, elapsed, _) in &timings {
            assert!(
                elapsed.as_secs() < 120,
                "pack_objects_revs for {count} commits too slow: {elapsed:?}"
            );
        }
    }

    // ── 3. incremental pack vs full ──────────────────────────────────────────

    #[tokio::test]
    async fn test_pack_incremental_vs_full() {
        let temp = TempTree::new("pack-incr-vs-full");
        let git = test_git();
        let (source_repo, source_sha) = create_source_repo_with_commits(&temp.path, 100);

        let cache_repo = temp.path.join("cache.git");
        git.init_bare(&cache_repo).await.unwrap();
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions::default(),
        )
        .await
        .unwrap();

        let head = CommitSha::parse(&source_sha).unwrap();

        // Full pack.
        let start = Instant::now();
        let full_pack = git
            .pack_objects_revs(
                &cache_repo,
                &temp.path.join("full-pack"),
                std::slice::from_ref(&head),
                &[],
            )
            .await
            .unwrap();
        let full_elapsed = start.elapsed();
        let full_size = std::fs::metadata(&full_pack).unwrap().len();

        // Get a mid-point commit for incremental base (exclude tip).
        let base_rev_str = git
            .rev_parse(&cache_repo, "refs/cache/main~50")
            .await
            .unwrap();
        let base_sha = CommitSha::parse(&base_rev_str).unwrap();

        // Incremental pack (tip minus the base commit).
        let start = Instant::now();
        let incr_pack = git
            .pack_objects_revs(
                &cache_repo,
                &temp.path.join("incr-pack"),
                std::slice::from_ref(&head),
                std::slice::from_ref(&base_sha),
            )
            .await
            .unwrap();
        let incr_elapsed = start.elapsed();
        let incr_size = std::fs::metadata(&incr_pack).unwrap().len();

        eprintln!(
            "pack full vs incremental (100 commits):\n  full:  {full_elapsed:?}, {full_size} bytes\n  incr:  {incr_elapsed:?}, {incr_size} bytes"
        );

        // Incremental pack should be smaller.
        assert!(
            incr_size < full_size,
            "incremental pack ({incr_size}) should be smaller than full ({full_size})"
        );
        assert!(
            full_elapsed.as_secs() < 120,
            "full pack too slow: {full_elapsed:?}"
        );
        assert!(
            incr_elapsed.as_secs() < 120,
            "incremental pack too slow: {incr_elapsed:?}"
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
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions::default(),
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
}
