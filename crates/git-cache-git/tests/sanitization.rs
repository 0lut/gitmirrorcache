//! Sanitization tests for the Git wrapper.
//!
//! Validates that every public method on the `Git` struct rejects flag-injection
//! (`-`-prefixed args) and NUL-byte-containing args, as required by AGENTS.md.

mod tests {
    use git_cache_git::Git;
    use std::path::Path;
    use std::time::Duration;

    fn test_git() -> Git {
        Git::default_with_timeout(Duration::from_secs(30))
    }

    // ── rev_parse sanitization ──────────────────────────────────────────────

    #[tokio::test]
    async fn rev_parse_rejects_dash_revision() {
        let git = test_git();
        assert!(
            git.rev_parse(Path::new("/unused"), "-evil").await.is_err(),
            "revision starting with '-' must be rejected"
        );
    }

    #[tokio::test]
    async fn rev_parse_rejects_double_dash_revision() {
        let git = test_git();
        assert!(
            git.rev_parse(Path::new("/unused"), "--flag").await.is_err(),
            "revision starting with '--' must be rejected"
        );
    }

    #[tokio::test]
    async fn rev_parse_rejects_nul_in_revision() {
        let git = test_git();
        assert!(
            git.rev_parse(Path::new("/unused"), "HEAD\0evil")
                .await
                .is_err(),
            "revision containing NUL must be rejected"
        );
    }

    #[tokio::test]
    async fn rev_parse_rejects_empty_revision() {
        let git = test_git();
        assert!(
            git.rev_parse(Path::new("/unused"), "").await.is_err(),
            "empty revision must be rejected"
        );
    }

    // ── update_ref sanitization ─────────────────────────────────────────────

    #[tokio::test]
    async fn update_ref_rejects_dash_ref_name() {
        let git = test_git();
        assert!(git
            .update_ref(Path::new("/unused"), "-evil", "abc123")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn update_ref_rejects_dash_sha() {
        let git = test_git();
        assert!(git
            .update_ref(Path::new("/unused"), "refs/heads/main", "-evil")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn update_ref_rejects_nul_in_ref_name() {
        let git = test_git();
        assert!(git
            .update_ref(Path::new("/unused"), "refs/heads\0bad", "abc123")
            .await
            .is_err());
    }

    // ── symbolic_ref sanitization ───────────────────────────────────────────

    #[tokio::test]
    async fn symbolic_ref_rejects_dash_name() {
        let git = test_git();
        assert!(git
            .symbolic_ref(Path::new("/unused"), "--evil", "refs/heads/main")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn symbolic_ref_rejects_dash_target() {
        let git = test_git();
        assert!(git
            .symbolic_ref(Path::new("/unused"), "HEAD", "-evil")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn symbolic_ref_rejects_nul_in_name() {
        let git = test_git();
        assert!(git
            .symbolic_ref(Path::new("/unused"), "HEAD\0x", "refs/heads/main")
            .await
            .is_err());
    }

    // ── set_config sanitization ─────────────────────────────────────────────

    #[tokio::test]
    async fn set_config_rejects_dash_key() {
        let git = test_git();
        assert!(git
            .set_config(Path::new("/unused"), "--evil", "value")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn set_config_rejects_nul_in_key() {
        let git = test_git();
        assert!(git
            .set_config(Path::new("/unused"), "key\0bad", "value")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn set_config_rejects_nul_in_value() {
        let git = test_git();
        assert!(git
            .set_config(Path::new("/unused"), "user.name", "value\0bad")
            .await
            .is_err());
    }

    // ── ls_remote_heads sanitization ────────────────────────────────────────

    #[tokio::test]
    async fn ls_remote_heads_rejects_dash_url() {
        let git = test_git();
        assert!(git.ls_remote_heads("-evil").await.is_err());
    }

    #[tokio::test]
    async fn ls_remote_heads_rejects_nul_url() {
        let git = test_git();
        assert!(git.ls_remote_heads("url\0bad").await.is_err());
    }

    #[tokio::test]
    async fn ls_remote_heads_rejects_empty_url() {
        let git = test_git();
        assert!(git.ls_remote_heads("").await.is_err());
    }

    // ── ls_remote_default_branch sanitization ───────────────────────────────

    #[tokio::test]
    async fn ls_remote_default_branch_rejects_dash_url() {
        let git = test_git();
        assert!(git.ls_remote_default_branch("-evil").await.is_err());
    }

    #[tokio::test]
    async fn ls_remote_default_branch_rejects_nul_url() {
        let git = test_git();
        assert!(git.ls_remote_default_branch("url\0bad").await.is_err());
    }

    // ── fetch_refspecs sanitization ─────────────────────────────────────────

    #[tokio::test]
    async fn fetch_refspecs_rejects_dash_url() {
        let git = test_git();
        assert!(git
            .fetch_refspecs(Path::new("/unused"), "-evil", &[], Default::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_refspecs_rejects_dash_refspec() {
        let git = test_git();
        assert!(git
            .fetch_refspecs(
                Path::new("/unused"),
                "https://example.com/repo.git",
                &["--upload-pack=evil".to_string()],
                Default::default()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_refspecs_rejects_nul_in_refspec() {
        let git = test_git();
        assert!(git
            .fetch_refspecs(
                Path::new("/unused"),
                "https://example.com/repo.git",
                &["bad\0spec".to_string()],
                Default::default()
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_refspecs_rejects_empty_refspec() {
        let git = test_git();
        assert!(git
            .fetch_refspecs(
                Path::new("/unused"),
                "https://example.com/repo.git",
                &["".to_string()],
                Default::default()
            )
            .await
            .is_err());
    }

    // ── branch_cache_refspec validation ─────────────────────────────────────

    #[test]
    fn branch_cache_refspec_accepts_normal_branch() {
        assert_eq!(
            git_cache_git::branch_cache_refspec("feature/foo-1.2").unwrap(),
            "+refs/heads/feature/foo-1.2:refs/cache/upstream/heads/feature/foo-1.2"
        );
    }

    #[test]
    fn branch_cache_refspec_rejects_colon_in_branch() {
        assert!(git_cache_git::branch_cache_refspec("evil:refs/heads/main").is_err());
    }

    #[test]
    fn branch_cache_refspec_rejects_nul_in_branch() {
        assert!(git_cache_git::branch_cache_refspec("bad\0branch").is_err());
    }

    #[test]
    fn branch_cache_refspec_rejects_glob_characters() {
        assert!(git_cache_git::branch_cache_refspec("*").is_err());
        assert!(git_cache_git::branch_cache_refspec("feature/*").is_err());
        assert!(git_cache_git::branch_cache_refspec("a?b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a[b]").is_err());
    }

    #[test]
    fn branch_cache_refspec_rejects_check_ref_format_violations() {
        assert!(git_cache_git::branch_cache_refspec("a..b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a@{b}").is_err());
        assert!(git_cache_git::branch_cache_refspec("@").is_err());
        assert!(git_cache_git::branch_cache_refspec("a b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a~b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a^b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a\\b").is_err());
        assert!(git_cache_git::branch_cache_refspec("a\x07b").is_err());
        assert!(git_cache_git::branch_cache_refspec(".hidden").is_err());
        assert!(git_cache_git::branch_cache_refspec("a/.b").is_err());
        assert!(git_cache_git::branch_cache_refspec("branch.lock").is_err());
        assert!(git_cache_git::branch_cache_refspec("a//b").is_err());
        assert!(git_cache_git::branch_cache_refspec("/leading").is_err());
        assert!(git_cache_git::branch_cache_refspec("trailing/").is_err());
        assert!(git_cache_git::branch_cache_refspec("trailing.").is_err());
    }

    // ── fetch_objects sanitization ──────────────────────────────────────────

    #[tokio::test]
    async fn fetch_objects_rejects_dash_url() {
        let git = test_git();
        assert!(git
            .fetch_objects(Path::new("/unused"), "-evil", &[], Default::default())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_objects_rejects_nul_in_url() {
        let git = test_git();
        assert!(git
            .fetch_objects(Path::new("/unused"), "url\0bad", &[], Default::default())
            .await
            .is_err());
    }
}
