//! Sanitization tests for the Git wrapper.
//!
//! Validates that every public method on the `Git` struct rejects flag-injection
//! (`-`-prefixed args) and NUL-byte-containing args, as required by AGENTS.md.

use git_cache_core::CommitSha;
use git_cache_git::Git;
use std::path::Path;
use std::time::Duration;

fn test_git() -> Git {
    Git::default_with_timeout(Duration::from_secs(30))
}

// ── fetch_branch sanitization ───────────────────────────────────────────

#[tokio::test]
async fn fetch_branch_rejects_dash_prefixed_branch() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "-evil", "refs/cache/test")
        .await;
    assert!(err.is_err(), "branch starting with '-' must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_double_dash_branch() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "--flag", "refs/cache/test")
        .await;
    assert!(err.is_err(), "branch starting with '--' must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_dash_prefixed_local_ref() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main", "-evil")
        .await;
    assert!(err.is_err(), "local_ref starting with '-' must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_double_dash_local_ref() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main", "--flag")
        .await;
    assert!(
        err.is_err(),
        "local_ref starting with '--' must be rejected"
    );
}

#[tokio::test]
async fn fetch_branch_rejects_nul_in_branch() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main\0evil", "refs/cache/test")
        .await;
    assert!(err.is_err(), "branch containing NUL must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_nul_in_local_ref() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main", "refs/cache\0evil")
        .await;
    assert!(err.is_err(), "local_ref containing NUL must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_empty_branch() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "", "refs/cache/test")
        .await;
    assert!(err.is_err(), "empty branch must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_empty_local_ref() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main", "")
        .await;
    assert!(err.is_err(), "empty local_ref must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_colon_in_branch() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "HEAD:path", "refs/cache/test")
        .await;
    assert!(err.is_err(), "branch containing ':' must be rejected");
}

#[tokio::test]
async fn fetch_branch_rejects_colon_in_local_ref() {
    let git = test_git();
    let err = git
        .fetch_branch(Path::new("/unused"), "url", "main", "refs:cache/test")
        .await;
    assert!(err.is_err(), "local_ref containing ':' must be rejected");
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

// ── bundle_create sanitization ──────────────────────────────────────────

#[tokio::test]
async fn bundle_create_rejects_dash_rev() {
    let git = test_git();
    assert!(
        git.bundle_create(Path::new("/unused"), Path::new("/unused.bundle"), "-evil")
            .await
            .is_err(),
        "rev starting with '-' must be rejected"
    );
}

#[tokio::test]
async fn bundle_create_rejects_double_dash_rev() {
    let git = test_git();
    assert!(
        git.bundle_create(Path::new("/unused"), Path::new("/unused.bundle"), "--flag")
            .await
            .is_err(),
        "rev starting with '--' must be rejected"
    );
}

#[tokio::test]
async fn bundle_create_rejects_nul_in_rev() {
    let git = test_git();
    assert!(
        git.bundle_create(
            Path::new("/unused"),
            Path::new("/unused.bundle"),
            "rev\0bad"
        )
        .await
        .is_err(),
        "rev containing NUL must be rejected"
    );
}

#[tokio::test]
async fn bundle_create_rejects_empty_rev() {
    let git = test_git();
    assert!(
        git.bundle_create(Path::new("/unused"), Path::new("/unused.bundle"), "")
            .await
            .is_err(),
        "empty rev must be rejected"
    );
}

// ── bundle_create_incremental sanitization ──────────────────────────────

#[tokio::test]
async fn bundle_create_incremental_rejects_dash_prefixed_sha() {
    // CommitSha::parse requires a 40-char hex string, so we can't construct
    // a dash-prefixed CommitSha directly. The validation in
    // bundle_create_incremental calls reject_revision_arg on each tip's
    // as_str(), and CommitSha already guarantees no dash prefix. This test
    // verifies the dual-layer defense by ensuring CommitSha::parse rejects
    // dash-prefixed input.
    let result = CommitSha::parse("-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(
        result.is_err(),
        "CommitSha::parse should reject dash-prefixed SHA"
    );
}

#[tokio::test]
async fn bundle_create_incremental_rejects_nul_in_sha() {
    // CommitSha::parse rejects NUL bytes since they aren't hex chars.
    let result = CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaa\0aaaaaaaaaaa");
    assert!(
        result.is_err(),
        "CommitSha::parse should reject NUL-containing SHA"
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

// ── fetch_refs sanitization ─────────────────────────────────────────────

#[tokio::test]
async fn fetch_refs_rejects_dash_url() {
    let git = test_git();
    assert!(git
        .fetch_refs(Path::new("/unused"), "-evil", &[])
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_refs_rejects_nul_in_url() {
    let git = test_git();
    assert!(git
        .fetch_refs(Path::new("/unused"), "url\0bad", &[])
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_refs_rejects_nul_in_refspec() {
    let git = test_git();
    assert!(git
        .fetch_refs(
            Path::new("/unused"),
            "https://example.com/repo.git",
            &["bad\0spec".to_string()]
        )
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_refs_rejects_empty_refspec() {
    let git = test_git();
    assert!(git
        .fetch_refs(
            Path::new("/unused"),
            "https://example.com/repo.git",
            &["".to_string()]
        )
        .await
        .is_err());
}
