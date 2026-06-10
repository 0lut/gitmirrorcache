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
                .join(format!("git-cache-git-{name}-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp tree");
            Self { path }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[tokio::test]
    async fn fetch_bundle_and_verify_local_bare_repos() {
        let temp = TempTree::new("bundle-flow");
        let (source_repo, source_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let hydrated_repo = temp.path.join("hydrated.git");
        let bundle_path = temp.path.join("cache.bundle");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch main into cache repo");

        let cached_sha = git
            .rev_parse(&cache_repo, "refs/cache/main^{commit}")
            .await
            .expect("resolve cached ref");
        assert_eq!(source_sha, cached_sha);

        git.fsck(&cache_repo).await.expect("fsck cache repo");
        git.bundle_create(&cache_repo, &bundle_path, "refs/cache/main")
            .await
            .expect("create cache bundle");
        assert!(bundle_path.is_file());

        git.init_bare(&hydrated_repo)
            .await
            .expect("init hydrated repo");
        git.fetch_bundle(&hydrated_repo, &bundle_path)
            .await
            .expect("fetch refs from bundle");

        let hydrated_sha = git
            .rev_parse(&hydrated_repo, "refs/cache/main^{commit}")
            .await
            .expect("resolve hydrated ref");
        assert_eq!(source_sha, hydrated_sha);

        git.fsck(&hydrated_repo).await.expect("fsck hydrated repo");
    }

    #[tokio::test]
    async fn repack_for_serving_writes_bitmap_index() {
        let temp = TempTree::new("repack-serving");
        let (source_repo, _) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch main into cache repo");

        git.repack_for_serving(&cache_repo)
            .await
            .expect("repack repo for serving");

        let pack_dir = cache_repo.join("objects/pack");
        let has_bitmap = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .any(|entry| {
                entry
                    .expect("pack dir entry")
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "bitmap")
            });
        assert!(
            has_bitmap,
            "expected a bitmap index in {}",
            pack_dir.display()
        );
    }

    #[tokio::test]
    async fn run_rejects_stdout_larger_than_limit() {
        let git = test_git().with_output_limit(1);
        let err = git
            .run(None, ["--version"])
            .await
            .expect_err("git --version should exceed one byte");

        assert!(
            err.to_string().contains("stdout exceeded limit"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn bundle_create_incremental_round_trips_from_base_bundle() {
        let temp = TempTree::new("incremental-bundle");
        let (source_repo, first_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let hydrated_repo = temp.path.join("hydrated.git");
        let full_bundle = temp.path.join("full.bundle");
        let delta_bundle = temp.path.join("delta.bundle");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch initial main");
        git.bundle_create_all(&cache_repo, &full_bundle)
            .await
            .expect("create full bundle");

        let second_sha = commit_source(&source_repo, "second");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch updated main");
        git.bundle_create_incremental(
            &cache_repo,
            &delta_bundle,
            &[CommitSha::parse(&first_sha).unwrap()],
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
        assert_eq!(second_sha, hydrated_sha);
        git.rev_parse(&hydrated_repo, &format!("{first_sha}^{{commit}}"))
            .await
            .expect("initial commit remains present");
        git.fsck(&hydrated_repo).await.expect("fsck hydrated repo");
    }

    #[tokio::test]
    async fn is_ancestor_reports_commit_reachability() {
        let temp = TempTree::new("is-ancestor");
        let (source_repo, first_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        let second_sha = commit_source(&source_repo, "second");
        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch main");

        let first = CommitSha::parse(&first_sha).unwrap();
        let second = CommitSha::parse(&second_sha).unwrap();
        assert!(git
            .is_ancestor(&cache_repo, &first, &second)
            .await
            .expect("check first ancestor of second"));
        assert!(!git
            .is_ancestor(&cache_repo, &second, &first)
            .await
            .expect("check second not ancestor of first"));
    }

    #[tokio::test]
    async fn object_reachable_from_commits_reports_blob_reachability() {
        let temp = TempTree::new("object-reachability");
        let (source_repo, source_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch main");

        let tip = CommitSha::parse(&source_sha).unwrap();
        let blob =
            CommitSha::parse(run_git(Some(&source_repo), ["rev-parse", "HEAD:README.md"])).unwrap();
        assert!(git
            .object_reachable_from_commits(&cache_repo, &blob, std::slice::from_ref(&tip))
            .await
            .expect("check blob reachability"));

        let unrelated = CommitSha::parse("f".repeat(40)).unwrap();
        assert!(!git
            .object_reachable_from_commits(&cache_repo, &unrelated, &[tip])
            .await
            .expect("check unrelated object reachability"));
    }

    #[tokio::test]
    async fn for_each_ref_commits_lists_matching_refs() {
        let temp = TempTree::new("for-each-ref");
        let (source_repo, source_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/upstream/heads/main",
        )
        .await
        .expect("fetch cache ref");

        let commits = git
            .for_each_ref_commits(&cache_repo, "refs/cache/upstream/heads")
            .await
            .expect("list cache refs");
        assert_eq!(commits, vec![CommitSha::parse(&source_sha).unwrap()]);
    }

    #[tokio::test]
    async fn for_each_ref_containing_commit_lists_matching_refs() {
        let temp = TempTree::new("for-each-ref-contains");
        let (source_repo, first_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        let second_sha = commit_source(&source_repo, "second");
        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/upstream/heads/main",
        )
        .await
        .expect("fetch cache ref");

        let first = CommitSha::parse(&first_sha).unwrap();
        let second = CommitSha::parse(&second_sha).unwrap();
        let first_containing = git
            .for_each_ref_containing_commit(&cache_repo, &first, &["refs/cache/upstream/heads"])
            .await
            .expect("list refs containing first commit");
        assert_eq!(first_containing, vec![second.clone()]);

        let second_containing = git
            .for_each_ref_containing_commit(&cache_repo, &second, &["refs/cache/upstream/heads"])
            .await
            .expect("list refs containing second commit");
        assert_eq!(second_containing, vec![second]);
    }

    #[tokio::test]
    async fn gitoxide_backend_matches_subprocess_backend() {
        let temp = TempTree::new("gitoxide-parity");
        let (source_repo, first_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let gix = test_git().with_gitoxide(true);
        let subprocess = test_git().with_gitoxide(false);

        let second_sha = commit_source(&source_repo, "second");
        gix.init_bare(&cache_repo).await.expect("init cache repo");
        gix.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/upstream/heads/main",
        )
        .await
        .expect("fetch cache ref");

        for rev in ["refs/cache/upstream/heads/main", second_sha.as_str()] {
            assert_eq!(
                gix.rev_parse(&cache_repo, rev)
                    .await
                    .expect("gix rev-parse"),
                subprocess
                    .rev_parse(&cache_repo, rev)
                    .await
                    .expect("subprocess rev-parse"),
            );
        }
        assert!(gix.rev_parse(&cache_repo, "refs/missing").await.is_err());

        assert_eq!(
            gix.for_each_ref(&cache_repo, "refs/cache/upstream/heads")
                .await
                .expect("gix for-each-ref"),
            subprocess
                .for_each_ref(&cache_repo, "refs/cache/upstream/heads")
                .await
                .expect("subprocess for-each-ref"),
        );

        let first = CommitSha::parse(&first_sha).unwrap();
        let second = CommitSha::parse(&second_sha).unwrap();
        for (ancestor, descendant) in [(&first, &second), (&second, &first), (&first, &first)] {
            assert_eq!(
                gix.is_ancestor(&cache_repo, ancestor, descendant)
                    .await
                    .expect("gix is-ancestor"),
                subprocess
                    .is_ancestor(&cache_repo, ancestor, descendant)
                    .await
                    .expect("subprocess is-ancestor"),
            );
        }

        let missing = CommitSha::parse("f".repeat(40)).unwrap();
        let ids = vec![first, second, missing];
        assert_eq!(
            gix.cat_file_batch_types(&cache_repo, &ids)
                .await
                .expect("gix cat-file types"),
            subprocess
                .cat_file_batch_types(&cache_repo, &ids)
                .await
                .expect("subprocess cat-file types"),
        );
    }

    #[tokio::test]
    async fn bundle_create_incremental_empty_excludes_creates_full_bundle() {
        let temp = TempTree::new("incremental-empty");
        let (source_repo, source_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let bundle_path = temp.path.join("cache.bundle");
        let hydrated_repo = temp.path.join("hydrated.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_branch(
            &cache_repo,
            path_arg(&source_repo),
            "main",
            "refs/cache/main",
        )
        .await
        .expect("fetch main");
        git.bundle_create_incremental(&cache_repo, &bundle_path, &[])
            .await
            .expect("create full bundle through incremental wrapper");

        git.init_bare(&hydrated_repo)
            .await
            .expect("init hydrated repo");
        git.fetch_bundle(&hydrated_repo, &bundle_path)
            .await
            .expect("fetch bundle");
        let hydrated_sha = git
            .rev_parse(&hydrated_repo, "refs/cache/main^{commit}")
            .await
            .expect("resolve hydrated ref");
        assert_eq!(source_sha, hydrated_sha);
    }

    fn test_git() -> Git {
        Git::default_with_timeout(Duration::from_secs(10))
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

    fn commit_source(source_repo: &Path, contents: &str) -> String {
        std::fs::write(source_repo.join("README.md"), format!("{contents}\n"))
            .expect("write README");
        run_git(Some(source_repo), ["add", "README.md"]);
        run_git(Some(source_repo), ["commit", "-m", contents]);
        run_git(Some(source_repo), ["rev-parse", "HEAD"])
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
}
