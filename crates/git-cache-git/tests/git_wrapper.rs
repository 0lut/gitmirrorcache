mod common;

mod tests {
    use git_cache_core::CommitSha;
    use git_cache_git::{FetchOptions, Git};
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
    async fn repack_for_serving_does_not_write_bitmap_index() {
        let temp = TempTree::new("repack-serving");
        let (source_repo, _) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions::default(),
        )
        .await
        .expect("fetch main into cache repo");

        git.repack_for_serving(&cache_repo)
            .await
            .expect("repack repo for serving");

        assert!(
            !pack_dir_has_bitmap(&cache_repo),
            "serving repack should avoid bitmap indexes because upload-pack disables bitmap traversal"
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
    async fn is_ancestor_reports_commit_reachability() {
        let temp = TempTree::new("is-ancestor");
        let (source_repo, first_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        let second_sha = commit_source(&source_repo, "second");
        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions::default(),
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
    async fn commit_history_complete_no_lazy_treats_shallow_repo_as_incomplete() {
        let temp = TempTree::new("history-complete-shallow");
        let (source_repo, _) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        let tip_sha = commit_source(&source_repo, "second");
        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            &format!("file://{}", path_arg(&source_repo)),
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions {
                depth: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("fetch shallow main");

        assert!(
            cache_repo.join("shallow").exists(),
            "test setup should leave the bare cache repo shallow"
        );
        assert!(
            !git.commit_history_complete_no_lazy(&cache_repo, &CommitSha::parse(&tip_sha).unwrap())
                .await
                .expect("check history completeness"),
            "a shallow repo is missing parent history even when rev-list would stop at the boundary"
        );
    }

    #[tokio::test]
    async fn commit_ancestry_window_stops_at_shallow_boundary() {
        let temp = TempTree::new("ancestry-window");
        let (source_repo, _) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        for message in ["second", "third", "fourth"] {
            commit_source(&source_repo, message);
        }
        let tip = CommitSha::parse(run_git(Some(&source_repo), ["rev-parse", "HEAD"])).unwrap();
        let source_url = format!("file://{}", path_arg(&source_repo));

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            &source_url,
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions {
                depth: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("fetch shallow main");

        // Cache holds two commits (tip + one parent); the parent is the
        // shallow boundary. A window deeper than the boundary truncates there
        // rather than lazily fetching the missing ancestors.
        let window = git
            .commit_ancestry_window_no_lazy(&cache_repo, &tip, 5)
            .await
            .expect("ancestry window");
        assert_eq!(window.len(), 2, "window stops at the shallow boundary");
        assert_eq!(window[0], tip);

        // A window within the present history returns exactly that many.
        let one = git
            .commit_ancestry_window_no_lazy(&cache_repo, &tip, 1)
            .await
            .expect("ancestry window");
        assert_eq!(one, vec![tip.clone()]);

        // An absent commit yields no window instead of erroring.
        let missing = CommitSha::parse("0000000000000000000000000000000000000000").unwrap();
        assert!(git
            .commit_ancestry_window_no_lazy(&cache_repo, &missing, 3)
            .await
            .expect("ancestry window for missing commit")
            .is_empty());
    }

    #[tokio::test]
    async fn deepen_extends_shallow_boundary_without_unshallowing() {
        let temp = TempTree::new("deepen-extends");
        let (source_repo, _) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        // Five commits upstream; a depth-1 fetch then a --deepen=2 must leave
        // the cache at three commits and still shallow (two commits short of
        // the full history), proving the deepen did not silently unshallow.
        for message in ["second", "third", "fourth", "fifth"] {
            commit_source(&source_repo, message);
        }
        let source_url = format!("file://{}", path_arg(&source_repo));

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            &source_url,
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions {
                depth: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("fetch shallow main");
        assert!(
            cache_repo.join("shallow").exists(),
            "depth-1 fetch is shallow"
        );
        assert_eq!(
            run_git(
                Some(&cache_repo),
                ["rev-list", "--count", "refs/cache/main"]
            ),
            "1"
        );

        git.fetch_ref(
            &cache_repo,
            &source_url,
            "refs/heads/main",
            "refs/cache/main",
            FetchOptions {
                deepen: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("deepen main");

        assert_eq!(
            run_git(
                Some(&cache_repo),
                ["rev-list", "--count", "refs/cache/main"]
            ),
            "3",
            "--deepen=2 should extend the shallow boundary by two commits"
        );
        assert!(
            cache_repo.join("shallow").exists(),
            "a bounded deepen short of full history must keep the cache shallow"
        );
    }

    #[tokio::test]
    async fn for_each_ref_commits_lists_matching_refs() {
        let temp = TempTree::new("for-each-ref");
        let (source_repo, source_sha) = create_source_repo(&temp.path);
        let cache_repo = temp.path.join("cache.git");
        let git = test_git();

        git.init_bare(&cache_repo).await.expect("init cache repo");
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/upstream/heads/main",
            FetchOptions::default(),
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
        git.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/upstream/heads/main",
            FetchOptions::default(),
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
        gix.fetch_ref(
            &cache_repo,
            path_arg(&source_repo),
            "refs/heads/main",
            "refs/cache/upstream/heads/main",
            FetchOptions::default(),
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

    fn pack_dir_has_bitmap(repo: &Path) -> bool {
        std::fs::read_dir(repo.join("objects/pack"))
            .expect("read pack dir")
            .any(|entry| {
                entry
                    .expect("pack dir entry")
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "bitmap")
            })
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
