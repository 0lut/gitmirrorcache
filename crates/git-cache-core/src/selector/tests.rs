use super::*;

#[test]
fn selector_matches_wire_format() {
    let selector: Selector = serde_json::from_str(r#"{"branch":"main"}"#).unwrap();
    assert_eq!(
        selector,
        Selector::Branch(BranchName::parse("main").unwrap())
    );
    assert_eq!(
        serde_json::to_string(&Selector::DefaultBranch).unwrap(),
        r#"{"default_branch":true}"#
    );
    assert_eq!(
        serde_json::to_string(&Selector::ShortCommit(
            ShortCommitSha::parse("abc123").unwrap()
        ))
        .unwrap(),
        r#"{"short_commit":"abc123"}"#
    );
}

#[test]
fn selector_requires_one_field() {
    assert!(
        serde_json::from_str::<Selector>(r#"{"branch":"main","default_branch":true}"#).is_err()
    );
    assert!(serde_json::from_str::<Selector>(
        r#"{"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","short_commit":"aaaaaaa"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<Selector>(r#"{}"#).is_err());
}

#[test]
fn branch_rejects_unsafe_refs() {
    for value in [
        "../main",
        "refs/heads/main",
        "main.lock",
        "feature//x",
        "bad:name",
    ] {
        assert!(BranchName::parse(value).is_err(), "{value}");
    }
}

// ── Additional BranchName correctness tests ──────────────────────

#[test]
fn branch_valid_simple_names() {
    assert!(BranchName::parse("main").is_ok());
    assert!(BranchName::parse("feature/foo").is_ok());
    assert!(BranchName::parse("release-1.0").is_ok());
}

#[test]
fn branch_rejects_empty() {
    assert!(BranchName::parse("").is_err());
}

#[test]
fn branch_rejects_leading_slash() {
    assert!(BranchName::parse("/main").is_err());
}

#[test]
fn branch_rejects_trailing_slash() {
    assert!(BranchName::parse("feature/").is_err());
}

#[test]
fn branch_rejects_dot_dot() {
    assert!(BranchName::parse("feature/..bad").is_err());
    assert!(BranchName::parse("..").is_err());
}

#[test]
fn branch_rejects_backslash() {
    assert!(BranchName::parse("feature\\bar").is_err());
}

#[test]
fn branch_rejects_control_chars() {
    assert!(BranchName::parse("main\x00").is_err());
    assert!(BranchName::parse("main\x07").is_err());
}

#[test]
fn branch_rejects_tilde_caret_question_star_bracket() {
    assert!(BranchName::parse("main~1").is_err());
    assert!(BranchName::parse("main^1").is_err());
    assert!(BranchName::parse("main?").is_err());
    assert!(BranchName::parse("main*").is_err());
    assert!(BranchName::parse("main[0]").is_err());
}

#[test]
fn branch_rejects_ending_dot_lock() {
    assert!(BranchName::parse("main.lock").is_err());
    assert!(BranchName::parse("feature/test.lock").is_err());
}

#[test]
fn branch_rejects_starting_refs() {
    assert!(BranchName::parse("refs/heads/main").is_err());
    assert!(BranchName::parse("refs/tags/v1").is_err());
}

#[test]
fn branch_ref_name_produces_full_ref() {
    let branch = BranchName::parse("main").unwrap();
    assert_eq!(branch.ref_name(), "refs/heads/main");
}

#[test]
fn branch_serde_round_trip() {
    let branch = BranchName::parse("feature/test").unwrap();
    let json = serde_json::to_string(&branch).unwrap();
    assert_eq!(json, r#""feature/test""#);
    let parsed: BranchName = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, branch);
}

// ── Additional Selector deserialization tests ────────────────────

#[test]
fn selector_deserializes_branch() {
    let s: Selector = serde_json::from_str(r#"{"branch":"main"}"#).unwrap();
    assert_eq!(s, Selector::Branch(BranchName::parse("main").unwrap()));
}

#[test]
fn selector_deserializes_commit() {
    let sha = "a".repeat(40);
    let json = format!(r#"{{"commit":"{sha}"}}"#);
    let s: Selector = serde_json::from_str(&json).unwrap();
    assert_eq!(s, Selector::Commit(CommitSha::parse(&sha).unwrap()));
}

#[test]
fn selector_deserializes_short_commit() {
    let s: Selector = serde_json::from_str(r#"{"short_commit":"abcdef"}"#).unwrap();
    assert_eq!(
        s,
        Selector::ShortCommit(ShortCommitSha::parse("abcdef").unwrap())
    );
}

#[test]
fn selector_deserializes_default_branch() {
    let s: Selector = serde_json::from_str(r#"{"default_branch":true}"#).unwrap();
    assert_eq!(s, Selector::DefaultBranch);
}

#[test]
fn selector_rejects_zero_fields() {
    assert!(serde_json::from_str::<Selector>(r#"{}"#).is_err());
}

#[test]
fn selector_rejects_multiple_fields() {
    assert!(
        serde_json::from_str::<Selector>(r#"{"branch":"main","default_branch":true}"#).is_err()
    );
}

#[test]
fn selector_serialization_round_trips() {
    let cases = [
        Selector::Branch(BranchName::parse("main").unwrap()),
        Selector::DefaultBranch,
        Selector::Commit(CommitSha::parse("b".repeat(40)).unwrap()),
        Selector::ShortCommit(ShortCommitSha::parse("abcdef12").unwrap()),
    ];
    for selector in &cases {
        let json = serde_json::to_string(selector).unwrap();
        let parsed: Selector = serde_json::from_str(&json).unwrap();
        assert_eq!(&parsed, selector);
    }
}
