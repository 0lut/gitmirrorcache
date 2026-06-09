mod tests {
    use super::super::*;

    #[test]
    fn object_keys_are_stable() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let commit = CommitSha::parse("a".repeat(40)).unwrap();
        assert_eq!(
                git_cache_objectstore::commit_manifest_key(&repo, &commit),
                "repos/github.com/org/repo/manifests/commits/aa/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.json"
            );
        assert_eq!(
            ref_manifest_key(&repo, "feature/test"),
            "repos/github.com/org/repo/manifests/refs/heads/feature%2Ftest.json"
        );
    }

    // ── Additional repo_from_git_path tests ────────────────────────

    #[test]
    fn repo_from_git_path_rejects_no_dot_git() {
        assert!(repo_from_git_path("github.com/org/repo").is_err());
    }

    #[test]
    fn repo_from_git_path_rejects_gitfoo_suffix() {
        assert!(repo_from_git_path("github.com/org/repo.gitfoo").is_err());
    }

    #[test]
    fn repo_from_git_path_with_info_refs_suffix() {
        let key = repo_from_git_path("github.com/org/repo.git/info/refs").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    #[test]
    fn repo_from_git_path_with_upload_pack_suffix() {
        let key = repo_from_git_path("github.com/org/repo.git/git-upload-pack").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    #[test]
    fn repo_from_git_path_bare_dot_git() {
        let key = repo_from_git_path("github.com/org/repo.git").unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
    }

    // ── validate_host tests ──────────────────────────────────────────

    #[tokio::test]
    async fn validate_host_accepts_allowed_host() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        assert!(materializer
            .validate_host(&RepoKey::parse("github.com/org/repo").unwrap())
            .is_ok());
    }

    #[tokio::test]
    async fn validate_host_rejects_unlisted_host() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        assert!(materializer
            .validate_host(&RepoKey::parse("evil.com/org/repo").unwrap())
            .is_err());
    }

    #[tokio::test]
    async fn upstream_url_with_upstream_root_set() {
        let fixture = GitFixture::new();
        let state = fixture.state();
        let materializer = Materializer::new(Arc::new(state));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let url = materializer.upstream_url(&repo).unwrap();
        // With upstream_root set, it should be a local path
        assert!(url.contains("github.com/org/repo.git"));
    }

    #[tokio::test]
    async fn upstream_url_without_upstream_root() {
        let fixture = GitFixture::new();
        let mut state = fixture.state();
        state.config.upstream_root = None;
        let materializer = Materializer::new(Arc::new(state));
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let url = materializer.upstream_url(&repo).unwrap();
        assert_eq!(url, "https://github.com/org/repo.git");
    }

    // ── default_manifest_key and bundle_key tests ────────────────────

    #[test]
    fn default_manifest_key_produces_expected_path() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        assert_eq!(
            default_manifest_key(&repo),
            "repos/github.com/org/repo/manifests/refs/default.json"
        );
    }

    #[test]
    fn bundle_key_produces_expected_path() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let gen = GenerationId::new();
        let key = bundle_key(&repo, gen);
        assert!(key.starts_with("repos/github.com/org/repo/generations/"));
        assert!(key.ends_with("/base.bundle"));
    }

    #[test]
    fn pending_generation_from_key_parses_scan_key() {
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let generation = GenerationId::new();
        let key = pending_generation_publish_key(&repo, generation);

        assert_eq!(
            pending_generation_from_key(&key).unwrap(),
            Some((repo, generation))
        );
        assert_eq!(pending_generation_from_key("other/key.json").unwrap(), None);
    }

    // ── synthesize_ref_advertisement tests ───────────────────────────

    #[test]
    fn repo_from_git_path_accepts_smart_http_suffixes() {
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git/info/refs")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
        assert_eq!(
            repo_from_git_path("github.com/astral-sh/uv.git/git-upload-pack")
                .unwrap()
                .as_str(),
            "github.com/astral-sh/uv"
        );
    }
}
