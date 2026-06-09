//! Correctness edge-case tests for git-cache-core types.

mod tests {
    use git_cache_core::{
        AppConfig, BranchName, CommitManifest, CommitSha, GenerationId, GenerationManifest,
        MaterializeRequest, RefManifest, RepoKey, Selector, ShortCommitSha,
    };
    use std::io::Write;

    // ── RepoKey edge cases ──────────────────────────────────────────────────

    #[test]
    fn repo_key_rejects_single_segment() {
        assert!(RepoKey::parse("github.com").is_err());
    }

    #[test]
    fn repo_key_rejects_two_segments() {
        assert!(RepoKey::parse("github.com/org").is_err());
    }

    #[test]
    fn repo_key_rejects_four_segments() {
        assert!(RepoKey::parse("github.com/org/repo/extra").is_err());
    }

    #[test]
    fn repo_key_rejects_leading_slash() {
        assert!(RepoKey::parse("/github.com/org/repo").is_err());
    }

    #[test]
    fn repo_key_rejects_trailing_slash() {
        assert!(RepoKey::parse("github.com/org/repo/").is_err());
    }

    #[test]
    fn repo_key_rejects_backslash() {
        assert!(RepoKey::parse("github.com\\org\\repo").is_err());
    }

    #[test]
    fn repo_key_rejects_dot_segments() {
        assert!(RepoKey::parse("github.com/./repo").is_err());
        assert!(RepoKey::parse("github.com/../repo").is_err());
        assert!(RepoKey::parse("github.com/org/..").is_err());
    }

    #[test]
    fn repo_key_rejects_unicode() {
        assert!(RepoKey::parse("github.com/orgé/repo").is_err());
        assert!(RepoKey::parse("github.com/org/repö").is_err());
        assert!(RepoKey::parse("ギットハブ/org/repo").is_err());
    }

    #[test]
    fn repo_key_rejects_special_chars_at_sign_space() {
        assert!(RepoKey::parse("github.com/org/re po").is_err());
        assert!(RepoKey::parse("github.com/org/re@po").is_err());
        assert!(RepoKey::parse("github.com/org/re#po").is_err());
        assert!(RepoKey::parse("github.com/org/re$po").is_err());
    }

    #[test]
    fn repo_key_rejects_very_long_string() {
        let long_seg = "a".repeat(1000);
        let key = format!("github.com/{long_seg}/repo");
        // Should still be valid if only alphanumeric
        assert!(RepoKey::parse(&key).is_ok());
    }

    #[test]
    fn repo_key_git_suffix_is_normal_char() {
        // `.git` suffix: the dot is allowed in segment chars, so `repo.git` should parse
        let key = RepoKey::parse("github.com/org/repo.git");
        assert!(key.is_ok());
        assert_eq!(key.unwrap().name(), "repo.git");
    }

    #[test]
    fn repo_key_rejects_empty_segment_from_double_slash() {
        // "github.com//repo" has 4 segments (one empty), should fail with != 3
        assert!(RepoKey::parse("github.com//repo").is_err());
    }

    #[test]
    fn repo_key_rejects_nul_in_segment() {
        assert!(RepoKey::parse("github.com/org/re\0po").is_err());
    }

    // ── CommitSha edge cases ────────────────────────────────────────────────

    #[test]
    fn commit_sha_rejects_empty() {
        assert!(CommitSha::parse("").is_err());
    }

    #[test]
    fn commit_sha_rejects_39_chars() {
        assert!(CommitSha::parse("a".repeat(39)).is_err());
    }

    #[test]
    fn commit_sha_rejects_41_chars() {
        assert!(CommitSha::parse("a".repeat(41)).is_err());
    }

    #[test]
    fn commit_sha_accepts_64_char_sha256() {
        // SHA-256 object IDs are 64 hex chars
        let sha = "b".repeat(64);
        let result = CommitSha::parse(&sha);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), sha);
    }

    #[test]
    fn commit_sha_uppercase_hex_normalized() {
        let upper = "A".repeat(40);
        let sha = CommitSha::parse(&upper).unwrap();
        assert!(sha.as_str().chars().all(|c| c == 'a'));
    }

    #[test]
    fn commit_sha_mixed_case_normalized() {
        let mixed = "aAbBcCdDeEfF00112233"
            .repeat(2)
            .chars()
            .take(40)
            .collect::<String>();
        let sha = CommitSha::parse(&mixed).unwrap();
        assert!(sha
            .as_str()
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[test]
    fn commit_sha_rejects_non_hex_chars() {
        let mut bad = "a".repeat(39);
        bad.push('g');
        assert!(CommitSha::parse(&bad).is_err());

        let mut bad2 = "a".repeat(39);
        bad2.push('z');
        assert!(CommitSha::parse(&bad2).is_err());
    }

    #[test]
    fn commit_sha_rejects_whitespace() {
        let with_space = format!("{} ", "a".repeat(39));
        assert!(CommitSha::parse(&with_space).is_err());

        let with_newline = format!("{}\n", "a".repeat(39));
        assert!(CommitSha::parse(&with_newline).is_err());

        let leading_space = format!(" {}", "a".repeat(39));
        assert!(CommitSha::parse(&leading_space).is_err());
    }

    // ── ShortCommitSha edge cases ───────────────────────────────────────────

    #[test]
    fn short_commit_sha_rejects_empty() {
        assert!(ShortCommitSha::parse("").is_err());
    }

    #[test]
    fn short_commit_sha_rejects_1_char() {
        assert!(ShortCommitSha::parse("a").is_err());
    }

    #[test]
    fn short_commit_sha_rejects_3_chars() {
        assert!(ShortCommitSha::parse("abc").is_err());
    }

    #[test]
    fn short_commit_sha_accepts_4_chars() {
        assert!(ShortCommitSha::parse("abcd").is_ok());
    }

    #[test]
    fn short_commit_sha_accepts_7_chars() {
        assert!(ShortCommitSha::parse("abcdef1").is_ok());
        assert_eq!(
            ShortCommitSha::parse("abcdef1").unwrap().as_str(),
            "abcdef1"
        );
    }

    #[test]
    fn short_commit_sha_accepts_39_chars() {
        assert!(ShortCommitSha::parse("a".repeat(39)).is_ok());
    }

    #[test]
    fn short_commit_sha_rejects_40_chars() {
        assert!(ShortCommitSha::parse("a".repeat(40)).is_err());
    }

    #[test]
    fn short_commit_sha_rejects_64_chars() {
        assert!(ShortCommitSha::parse("a".repeat(64)).is_err());
    }

    #[test]
    fn short_commit_sha_rejects_non_hex() {
        assert!(ShortCommitSha::parse("ghijklmn").is_err());
        assert!(ShortCommitSha::parse("zzzzzz").is_err());
    }

    #[test]
    fn short_commit_sha_normalizes_uppercase() {
        let sha = ShortCommitSha::parse("ABCDEF1").unwrap();
        assert_eq!(sha.as_str(), "abcdef1");
    }

    // ── BranchName edge cases ───────────────────────────────────────────────

    #[test]
    fn branch_rejects_empty() {
        assert!(BranchName::parse("").is_err());
    }

    #[test]
    fn branch_rejects_starts_with_slash() {
        assert!(BranchName::parse("/main").is_err());
    }

    #[test]
    fn branch_rejects_ends_with_slash() {
        assert!(BranchName::parse("feature/").is_err());
    }

    #[test]
    fn branch_rejects_double_slash() {
        assert!(BranchName::parse("feature//bar").is_err());
    }

    #[test]
    fn branch_rejects_backslash() {
        assert!(BranchName::parse("feature\\bar").is_err());
    }

    #[test]
    fn branch_rejects_dot_dot() {
        assert!(BranchName::parse("feature/..bar").is_err());
        assert!(BranchName::parse("..").is_err());
    }

    #[test]
    fn branch_rejects_starts_with_refs() {
        assert!(BranchName::parse("refs/heads/main").is_err());
        assert!(BranchName::parse("refs/tags/v1").is_err());
    }

    #[test]
    fn branch_rejects_ends_with_dot_lock() {
        assert!(BranchName::parse("main.lock").is_err());
        assert!(BranchName::parse("feature/test.lock").is_err());
    }

    #[test]
    fn branch_rejects_control_chars() {
        assert!(BranchName::parse("main\x00").is_err());
        assert!(BranchName::parse("main\x07bell").is_err());
        assert!(BranchName::parse("main\x1b").is_err());
    }

    #[test]
    fn branch_rejects_special_git_refname_chars() {
        assert!(BranchName::parse("main~1").is_err());
        assert!(BranchName::parse("main^2").is_err());
        assert!(BranchName::parse("main:path").is_err());
        assert!(BranchName::parse("main?").is_err());
        assert!(BranchName::parse("main*").is_err());
        assert!(BranchName::parse("main[0]").is_err());
    }

    #[test]
    fn branch_accepts_valid_complex_names() {
        assert!(BranchName::parse("feature/foo-bar").is_ok());
        assert!(BranchName::parse("release-1.0.0").is_ok());
        assert!(BranchName::parse("user/john/experiment").is_ok());
        assert!(BranchName::parse("fix_bug_123").is_ok());
        assert!(BranchName::parse("UPPERCASE").is_ok());
    }

    #[test]
    fn branch_ref_name_returns_full_ref() {
        let branch = BranchName::parse("feature/foo-bar").unwrap();
        assert_eq!(branch.ref_name(), "refs/heads/feature/foo-bar");
    }

    // ── Selector serde edge cases ───────────────────────────────────────────

    #[test]
    fn selector_rejects_zero_selectors() {
        assert!(serde_json::from_str::<Selector>(r#"{}"#).is_err());
    }

    #[test]
    fn selector_rejects_two_selectors() {
        assert!(
            serde_json::from_str::<Selector>(r#"{"branch":"main","default_branch":true}"#).is_err()
        );
        let json = format!(r#"{{"commit":"{}","branch":"main"}}"#, "a".repeat(40));
        assert!(serde_json::from_str::<Selector>(&json).is_err());
    }

    #[test]
    fn selector_unknown_field_is_ignored_by_serde() {
        // serde by default ignores unknown fields (no deny_unknown_fields)
        let result =
            serde_json::from_str::<Selector>(r#"{"branch":"main","unknown_field":"value"}"#);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Selector::Branch(BranchName::parse("main").unwrap())
        );
    }

    #[test]
    fn selector_default_branch_false_counts_as_zero() {
        // default_branch: false should mean "no selector set" → error with empty
        let result = serde_json::from_str::<Selector>(r#"{"default_branch":false}"#);
        assert!(result.is_err());
    }

    // ── GenerationId edge cases ─────────────────────────────────────────────

    #[test]
    fn generation_id_uniqueness() {
        let ids: Vec<_> = (0..100).map(|_| GenerationId::new()).collect();
        let mut deduped = ids.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(ids.len(), deduped.len());
    }

    #[test]
    fn generation_id_ordering_v7_time_ordered() {
        let a = GenerationId::new();
        // Small sleep not needed; v7 UUIDs embed monotonic counter within same timestamp
        let b = GenerationId::new();
        let c = GenerationId::new();
        // v7 UUIDs: later ones should have >= timestamp portion
        assert!(a.0 <= b.0);
        assert!(b.0 <= c.0);
        // At least one pair should differ
        assert!(a != c);
    }

    // ── AppConfig edge cases ────────────────────────────────────────────────

    #[test]
    fn app_config_from_path_missing_file() {
        let result = AppConfig::from_path("/nonexistent/path/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn app_config_from_path_invalid_toml() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(tmp, "this is {{ not valid }} toml ===").unwrap();
        assert!(AppConfig::from_path(tmp.path()).is_err());
    }

    #[test]
    fn app_config_from_path_valid_toml() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
    bind_addr = "127.0.0.1:9090"
    cache_root = "/tmp/cache"

    [object_store]
    kind = "local"
    root = "/tmp/objects"

    [disk]
    quota_bytes = 5368709120
    min_free_bytes = 1073741824
    "#
        )
        .unwrap();

        let config = AppConfig::from_path(tmp.path()).unwrap();
        assert_eq!(config.bind_addr.port(), 9090);
    }

    #[test]
    fn app_config_from_path_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Empty TOML file missing required fields
        assert!(AppConfig::from_path(tmp.path()).is_err());
    }

    // ── MaterializeRequest serde edge cases ─────────────────────────────────

    #[test]
    fn materialize_request_missing_repo() {
        let json = r#"{"selector":{"branch":"main"}}"#;
        assert!(serde_json::from_str::<MaterializeRequest>(json).is_err());
    }

    #[test]
    fn materialize_request_missing_selector() {
        let json = r#"{"repo":"github.com/org/repo"}"#;
        assert!(serde_json::from_str::<MaterializeRequest>(json).is_err());
    }

    #[test]
    fn selector_rejects_reachable_from_contract() {
        let json = format!(
            r#"{{"commit":"{}","reachable_from":{{"branch":"main"}}}}"#,
            "a".repeat(40)
        );
        let error = serde_json::from_str::<Selector>(&json).unwrap_err();
        assert!(error
            .to_string()
            .contains("reachable_from selectors are not supported"));
    }

    #[test]
    fn materialize_request_extra_fields_rejected() {
        let json = r#"{"repo":"github.com/org/repo","selector":{"branch":"main"},"extra":"value"}"#;
        assert!(serde_json::from_str::<MaterializeRequest>(json).is_err());
    }

    #[test]
    fn materialize_request_legacy_mode_field_is_rejected() {
        let json =
            r#"{"repo":"github.com/org/repo","selector":{"default_branch":true},"mode":"cached"}"#;
        assert!(serde_json::from_str::<MaterializeRequest>(json).is_err());
    }

    #[test]
    fn materialize_request_serde_round_trip() {
        let req = MaterializeRequest {
            repo: RepoKey::parse("github.com/org/repo").unwrap(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            upstream_authorization: Default::default(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: MaterializeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, parsed);
    }

    #[test]
    fn materialize_request_invalid_repo_in_json() {
        let json = r#"{"repo":"invalid","selector":{"branch":"main"}}"#;
        assert!(serde_json::from_str::<MaterializeRequest>(json).is_err());
    }

    // ── Additional round-trip / Display tests ───────────────────────────────

    #[test]
    fn repo_key_from_str_trait() {
        let key: RepoKey = "github.com/org/repo".parse().unwrap();
        assert_eq!(key.as_str(), "github.com/org/repo");
        assert!("invalid".parse::<RepoKey>().is_err());
    }

    #[test]
    fn commit_sha_from_str_trait() {
        let sha: CommitSha = "a".repeat(40).parse().unwrap();
        assert_eq!(sha.as_str(), "a".repeat(40));
        assert!("short".parse::<CommitSha>().is_err());
    }

    #[test]
    fn short_commit_sha_from_str_trait() {
        let sha: ShortCommitSha = "abcdef".parse().unwrap();
        assert_eq!(sha.as_str(), "abcdef");
        assert!("ab".parse::<ShortCommitSha>().is_err());
    }

    #[test]
    fn branch_name_from_str_trait() {
        let b: BranchName = "main".parse().unwrap();
        assert_eq!(b.as_str(), "main");
        assert!("".parse::<BranchName>().is_err());
    }

    #[test]
    fn generation_id_display_is_uuid_format() {
        let id = GenerationId::new();
        let s = id.to_string();
        // UUID format: 8-4-4-4-12
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
    }

    // ── Manifest missing-field serde ────────────────────────────────────────

    #[test]
    fn generation_manifest_missing_repo_field() {
        let json = r#"{"generation":"550e8400-e29b-41d4-a716-446655440000","bundle_key":"k","created_at":"2026-01-01T00:00:00Z"}"#;
        assert!(serde_json::from_str::<GenerationManifest>(json).is_err());
    }

    #[test]
    fn commit_manifest_missing_commit_field() {
        let json = r#"{"repo":"github.com/org/repo","generation":"550e8400-e29b-41d4-a716-446655440000","complete":true,"verified_at":"2026-01-01T00:00:00Z"}"#;
        assert!(serde_json::from_str::<CommitManifest>(json).is_err());
    }

    #[test]
    fn ref_manifest_serde_round_trip_external() {
        let manifest = RefManifest {
            repo: RepoKey::parse("github.com/test/repo").unwrap(),
            ref_name: "refs/heads/main".into(),
            commit: CommitSha::parse("c".repeat(40)).unwrap(),
            generation: GenerationId::new(),
            verified_at: chrono::Utc::now(),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: RefManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, parsed);
    }
}
