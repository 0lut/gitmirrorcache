//! Load tests simulating large repository behavior for the Git wrapper.

mod tests {
    use git_cache_core::CommitSha;
    use git_cache_git::Git;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    struct TempTree {
        path: PathBuf,
    }

    impl TempTree {
        fn new(name: &str) -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("git-cache-load-{name}-{}-{id}", std::process::id()));
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
        Git::default_with_timeout(Duration::from_secs(120))
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

        for attempt in 0..3 {
            let output = command.output().expect("run setup git command");
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).trim().to_string();
            }
            if attempt < 2 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                // Rebuild the command since output consumed it
                command = Command::new("git");
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
                continue;
            }
            panic!(
                "git {:?} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
                args,
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        unreachable!()
    }

    fn path_arg(path: &Path) -> &str {
        path.to_str().expect("test paths are utf-8")
    }

    fn create_repo_with_n_commits(root: &Path, name: &str, n: usize) -> (PathBuf, PathBuf, String) {
        let bare_repo = root.join(format!("{name}.git"));
        let work_dir = root.join(format!("{name}-work"));

        run_git(None, ["init", "--bare", "--", path_arg(&bare_repo)]);
        run_git(None, ["init", "--", path_arg(&work_dir)]);
        run_git(Some(&work_dir), ["checkout", "-B", "main"]);
        run_git(
            Some(&work_dir),
            ["config", "user.email", "test@example.invalid"],
        );
        run_git(Some(&work_dir), ["config", "user.name", "Load Test"]);
        run_git(
            Some(&work_dir),
            ["remote", "add", "origin", path_arg(&bare_repo)],
        );

        for i in 0..n {
            std::fs::write(work_dir.join("data.txt"), format!("commit {i}\n")).expect("write data");
            run_git(Some(&work_dir), ["add", "data.txt"]);
            run_git(Some(&work_dir), ["commit", "-m", &format!("commit {i}")]);
        }

        run_git(Some(&work_dir), ["push", "origin", "main"]);
        run_git(
            Some(&bare_repo),
            ["symbolic-ref", "HEAD", "refs/heads/main"],
        );
        let head_sha = run_git(Some(&work_dir), ["rev-parse", "HEAD"]);
        (bare_repo, work_dir, head_sha)
    }

    fn create_repo_with_n_branches(root: &Path, name: &str, n: usize) -> (PathBuf, PathBuf) {
        let bare_repo = root.join(format!("{name}.git"));
        let work_dir = root.join(format!("{name}-work"));

        run_git(None, ["init", "--bare", "--", path_arg(&bare_repo)]);
        run_git(None, ["init", "--", path_arg(&work_dir)]);
        run_git(Some(&work_dir), ["checkout", "-B", "main"]);
        run_git(
            Some(&work_dir),
            ["config", "user.email", "test@example.invalid"],
        );
        run_git(Some(&work_dir), ["config", "user.name", "Load Test"]);
        run_git(
            Some(&work_dir),
            ["remote", "add", "origin", path_arg(&bare_repo)],
        );

        std::fs::write(work_dir.join("base.txt"), "base\n").expect("write base");
        run_git(Some(&work_dir), ["add", "base.txt"]);
        run_git(Some(&work_dir), ["commit", "-m", "base commit"]);
        run_git(Some(&work_dir), ["push", "origin", "main"]);

        for i in 0..n {
            let branch_name = format!("feature-{i}");
            run_git(Some(&work_dir), ["checkout", "-B", &branch_name, "main"]);
            std::fs::write(work_dir.join("branch.txt"), format!("branch {i}\n"))
                .expect("write branch");
            run_git(Some(&work_dir), ["add", "branch.txt"]);
            run_git(
                Some(&work_dir),
                ["commit", "-m", &format!("commit on {branch_name}")],
            );
            run_git(Some(&work_dir), ["push", "origin", &branch_name]);
        }

        run_git(Some(&work_dir), ["checkout", "main"]);
        run_git(
            Some(&bare_repo),
            ["symbolic-ref", "HEAD", "refs/heads/main"],
        );

        (bare_repo, work_dir)
    }

    // ── 1. Bundle large repo (100 commits) ──────────────────────────────────

    #[tokio::test]
    async fn bundle_large_repo() {
        let temp = TempTree::new("bundle-large");
        let (bare_repo, _work_dir, _head_sha) =
            create_repo_with_n_commits(&temp.path, "large", 100);

        let cache_repo = temp.path.join("cache.git");
        let bundle_path = temp.path.join("large.bundle");
        let hydrated_repo = temp.path.join("hydrated.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(&cache_repo, path_arg(&bare_repo), "main", "refs/cache/main")
            .await
            .expect("fetch main into cache repo");

        git.bundle_create(&cache_repo, &bundle_path, "refs/cache/main")
            .await
            .expect("create bundle from 500-commit repo");
        assert!(bundle_path.is_file(), "bundle file should exist");

        git.init_bare(&hydrated_repo)
            .await
            .expect("init hydrated repo");
        git.fetch_bundle(&hydrated_repo, &bundle_path)
            .await
            .expect("fetch bundle into hydrated repo");
        git.fsck(&hydrated_repo)
            .await
            .expect("fsck hydrated repo after large bundle");
    }

    // ── 2. Incremental bundle chain ─────────────────────────────────────────

    #[tokio::test]
    async fn incremental_bundle_chain() {
        let temp = TempTree::new("incr-bundle-chain");
        let (bare_repo, work_dir, first_head) = create_repo_with_n_commits(&temp.path, "incr", 100);

        let cache_repo = temp.path.join("cache.git");
        let full_bundle = temp.path.join("full.bundle");
        let delta_bundle = temp.path.join("delta.bundle");
        let hydrated_repo = temp.path.join("hydrated.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(&cache_repo, path_arg(&bare_repo), "main", "refs/cache/main")
            .await
            .expect("fetch initial 100 commits");

        git.bundle_create_all(&cache_repo, &full_bundle)
            .await
            .expect("create full bundle");

        for i in 0..50 {
            std::fs::write(work_dir.join("data.txt"), format!("extra commit {i}\n"))
                .expect("write extra");
            run_git(Some(&work_dir), ["add", "data.txt"]);
            run_git(
                Some(&work_dir),
                ["commit", "-m", &format!("extra commit {i}")],
            );
        }
        run_git(Some(&work_dir), ["push", "origin", "main"]);

        git.fetch_branch(&cache_repo, path_arg(&bare_repo), "main", "refs/cache/main")
            .await
            .expect("fetch 50 new commits");

        git.bundle_create_incremental(
            &cache_repo,
            &delta_bundle,
            &[CommitSha::parse(&first_head).unwrap()],
        )
        .await
        .expect("create incremental bundle");

        git.init_bare(&hydrated_repo)
            .await
            .expect("init hydrated repo");
        git.fetch_bundle(&hydrated_repo, &full_bundle)
            .await
            .expect("fetch full bundle");
        git.fetch_bundle(&hydrated_repo, &delta_bundle)
            .await
            .expect("fetch delta bundle");

        let hydrated_sha = git
            .rev_parse(&hydrated_repo, "refs/cache/main^{commit}")
            .await
            .expect("resolve hydrated ref");

        let expected_sha = run_git(Some(&work_dir), ["rev-parse", "HEAD"]);
        assert_eq!(hydrated_sha, expected_sha);

        git.rev_parse(&hydrated_repo, &format!("{first_head}^{{commit}}"))
            .await
            .expect("initial commit still present");

        let count = run_git(
            Some(&hydrated_repo),
            ["rev-list", "--count", "refs/cache/main"],
        );
        assert_eq!(
            count.parse::<usize>().unwrap(),
            150,
            "should have all 150 commits"
        );

        git.fsck(&hydrated_repo).await.expect("fsck hydrated repo");
    }

    // ── 3. Many branches bundle ─────────────────────────────────────────────

    #[tokio::test]
    async fn many_branches_bundle() {
        let temp = TempTree::new("many-branches-bundle");
        let (bare_repo, _work_dir) = create_repo_with_n_branches(&temp.path, "branchy", 50);

        let cache_repo = temp.path.join("cache.git");
        let bundle_path = temp.path.join("branches.bundle");
        let hydrated_repo = temp.path.join("hydrated.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");

        // Fetch all branches into cache
        for i in 0..50 {
            let branch = format!("feature-{i}");
            let local_ref = format!("refs/cache/{branch}");
            git.fetch_branch(&cache_repo, path_arg(&bare_repo), &branch, &local_ref)
                .await
                .unwrap_or_else(|e| panic!("fetch branch {branch}: {e}"));
        }
        git.fetch_branch(&cache_repo, path_arg(&bare_repo), "main", "refs/cache/main")
            .await
            .expect("fetch main");

        git.bundle_create_all(&cache_repo, &bundle_path)
            .await
            .expect("bundle_create_all with many branches");

        git.init_bare(&hydrated_repo)
            .await
            .expect("init hydrated repo");
        git.fetch_bundle(&hydrated_repo, &bundle_path)
            .await
            .expect("fetch bundle with many branches");

        // Verify all branches are present
        let refs_output = run_git(
            Some(&hydrated_repo),
            ["for-each-ref", "--format=%(refname)"],
        );
        for i in 0..50 {
            let expected_ref = format!("refs/cache/feature-{i}");
            assert!(
                refs_output.contains(&expected_ref),
                "missing ref {expected_ref} in hydrated repo"
            );
        }

        git.fsck(&hydrated_repo).await.expect("fsck hydrated repo");
    }

    // ── 4. Concurrent fetch_branch ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_fetch_branch() {
        let temp = TempTree::new("concurrent-fetch");
        let (bare_repo, _work_dir, _head_sha) =
            create_repo_with_n_commits(&temp.path, "source", 20);

        let git = test_git();
        let bare_url = path_arg(&bare_repo).to_string();

        let mut handles = Vec::new();
        for i in 0..10 {
            let git = git.clone();
            let url = bare_url.clone();
            let root = temp.path.to_path_buf();
            handles.push(tokio::spawn(async move {
                let cache_repo = root.join(format!("cache-{i}.git"));
                git.init_bare(&cache_repo).await.expect("init cache repo");
                git.fetch_branch(&cache_repo, &url, "main", "refs/cache/main")
                    .await
                    .expect("concurrent fetch_branch should succeed");
                git.fsck(&cache_repo).await.expect("fsck cache repo");
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }
}
