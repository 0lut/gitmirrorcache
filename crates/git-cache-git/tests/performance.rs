//! Performance tests for the Git wrapper.

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
            let path = std::env::temp_dir()
                .join(format!("git-cache-perf-{name}-{}-{id}", std::process::id()));
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
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_0", "gc.auto")
            .env("GIT_CONFIG_VALUE_0", "0")
            .env("GIT_CONFIG_KEY_1", "gc.autoDetach")
            .env("GIT_CONFIG_VALUE_1", "false")
            .env("GIT_CONFIG_KEY_2", "maintenance.auto")
            .env("GIT_CONFIG_VALUE_2", "false")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("HOME", "/nonexistent");

        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
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

    // ── 1. init_bare throughput ──────────────────────────────────────────────

    #[tokio::test]
    async fn test_init_bare_throughput() {
        let temp = TempTree::new("init-bare-throughput");
        let git = test_git();
        let count = 50;

        let start = Instant::now();
        for i in 0..count {
            let repo_dir = temp.path.join(format!("repo-{i}.git"));
            git.init_bare(&repo_dir).await.unwrap();
        }
        let elapsed = start.elapsed();

        let ops_per_sec = count as f64 / elapsed.as_secs_f64();
        eprintln!("init_bare throughput: {count} repos in {elapsed:?} ({ops_per_sec:.0} ops/sec)");
        assert!(
            elapsed.as_secs() < 30,
            "init_bare throughput too slow: {elapsed:?}"
        );
    }

    // ── 2. Pack create/restore cycle ─────────────────────────────────────────

    #[tokio::test]
    async fn test_pack_create_restore_cycle() {
        let temp = TempTree::new("pack-cycle");
        let git = test_git();
        let (source_repo, source_sha) = create_source_repo_with_commits(&temp.path, 50);

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
                &temp.path.join("perf-pack"),
                std::slice::from_ref(&head),
                &[],
            )
            .await
            .unwrap();
        let pack_elapsed = start.elapsed();
        assert!(pack_path.is_file());

        let restore_repo = temp.path.join("restored.git");
        git.init_bare(&restore_repo).await.unwrap();

        let start = Instant::now();
        let pack_dir = restore_repo.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let final_path = pack_dir.join("pack-perf.pack");
        std::fs::copy(&pack_path, &final_path).unwrap();
        git.index_pack(&restore_repo, &final_path).await.unwrap();
        git.update_refs_batch(&restore_repo, &[("refs/cache/main".to_string(), head)])
            .await
            .unwrap();
        let restore_elapsed = start.elapsed();

        let restored_sha = git
            .rev_parse(&restore_repo, "refs/cache/main^{commit}")
            .await
            .unwrap();
        assert_eq!(source_sha, restored_sha);

        let total = pack_elapsed + restore_elapsed;
        eprintln!(
            "pack create/restore: create={pack_elapsed:?}, restore={restore_elapsed:?}, total={total:?} (50 commits)"
        );
        assert!(total.as_secs() < 30, "pack cycle too slow: {total:?}");
    }

    // ── 3. rev_parse latency ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_rev_parse_latency() {
        let temp = TempTree::new("rev-parse-perf");
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

        let iterations = 100;
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
        eprintln!("rev_parse: {iterations} calls in {elapsed:?}, avg={avg:?}");
        assert!(
            avg.as_millis() < 500,
            "rev_parse average latency too high: {avg:?}"
        );
    }

    // ── 5. fsck throughput ───────────────────────────────────────────────────

    #[tokio::test]
    async fn test_fsck_throughput() {
        let temp = TempTree::new("fsck-throughput");
        let git = test_git();
        let (source_repo, _) = create_source_repo_with_commits(&temp.path, 20);

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

        let iterations = 10;
        let start = Instant::now();
        for _ in 0..iterations {
            git.fsck(&cache_repo).await.unwrap();
        }
        let elapsed = start.elapsed();

        let avg = elapsed / iterations;
        eprintln!("fsck: {iterations} calls in {elapsed:?}, avg={avg:?}");
        assert!(avg.as_secs() < 5, "fsck average latency too high: {avg:?}");
    }
}
