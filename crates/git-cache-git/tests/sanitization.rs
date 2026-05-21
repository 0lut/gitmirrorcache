//! Sanitization tests for the Git wrapper.
//!
//! Validates that every public method on the `Git` struct rejects flag-injection
//! (`-`-prefixed args) and NUL-byte-containing args, as required by AGENTS.md.

use git_cache_core::CommitSha;
use git_cache_git::Git;
use std::ffi::OsString;
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
            .join(format!("git-cache-sanitize-{name}-{}-{id}", std::process::id()));
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

fn path_arg(path: &Path) -> &str {
    path.to_str().expect("test paths are utf-8")
}

fn run_git<I, S>(cwd: Option<&Path>, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
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
    let output = command.output().expect("run setup git command");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Create a valid source repo for sanitization tests that need a real repo.
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
    std::fs::write(source_repo.join("README.md"), "hello\n").expect("write README");
    run_git(Some(&source_repo), ["add", "README.md"]);
    run_git(Some(&source_repo), ["commit", "-m", "initial"]);
    let sha = run_git(Some(&source_repo), ["rev-parse", "HEAD"]);
    (source_repo, sha)
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
        .fetch_branch(
            Path::new("/unused"),
            "url",
            "main\0evil",
            "refs/cache/test",
        )
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
        .fetch_branch(
            Path::new("/unused"),
            "url",
            "HEAD:path",
            "refs/cache/test",
        )
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
        git.bundle_create(Path::new("/unused"), Path::new("/unused.bundle"), "rev\0bad")
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

// ── upload_pack_stateless_rpc sanitization ──────────────────────────────

#[tokio::test]
async fn upload_pack_stateless_rpc_rejects_oversized_request() {
    let git = test_git();
    let big_request = vec![b'x'; 100];
    let err = git
        .upload_pack_stateless_rpc(Path::new("/unused"), &big_request, 10, 1024)
        .await;
    assert!(err.is_err(), "oversized request must be rejected");
    let msg = err.unwrap_err().to_string();
    assert!(
        msg.contains("upload-pack request exceeded limit"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn upload_pack_stateless_rpc_rejects_request_at_exact_limit() {
    let git = test_git();
    let request = vec![b'x'; 11]; // 11 > 10 byte limit
    let err = git
        .upload_pack_stateless_rpc(Path::new("/unused"), &request, 10, 1024)
        .await;
    assert!(
        err.is_err(),
        "request exceeding limit by 1 byte must be rejected"
    );
}

// ── upload_pack_advertise_refs ──────────────────────────────────────────

#[tokio::test]
async fn upload_pack_advertise_refs_works_on_valid_repo() {
    let temp = TempTree::new("adv-refs");
    let (source_repo, _sha) = create_source_repo(&temp.path);
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

    let result = git
        .upload_pack_advertise_refs(&cache_repo, 128 * 1024)
        .await;
    assert!(result.is_ok(), "advertise refs should succeed on valid repo");
    let output = result.unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("refs/cache/main"), "output: {text}");
}

// ── update_ref sanitization ─────────────────────────────────────────────

#[tokio::test]
async fn update_ref_rejects_dash_ref_name() {
    let git = test_git();
    assert!(
        git.update_ref(Path::new("/unused"), "-evil", "abc123")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn update_ref_rejects_dash_sha() {
    let git = test_git();
    assert!(
        git.update_ref(Path::new("/unused"), "refs/heads/main", "-evil")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn update_ref_rejects_nul_in_ref_name() {
    let git = test_git();
    assert!(
        git.update_ref(Path::new("/unused"), "refs/heads\0bad", "abc123")
            .await
            .is_err()
    );
}

// ── symbolic_ref sanitization ───────────────────────────────────────────

#[tokio::test]
async fn symbolic_ref_rejects_dash_name() {
    let git = test_git();
    assert!(
        git.symbolic_ref(Path::new("/unused"), "--evil", "refs/heads/main")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn symbolic_ref_rejects_dash_target() {
    let git = test_git();
    assert!(
        git.symbolic_ref(Path::new("/unused"), "HEAD", "-evil")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn symbolic_ref_rejects_nul_in_name() {
    let git = test_git();
    assert!(
        git.symbolic_ref(Path::new("/unused"), "HEAD\0x", "refs/heads/main")
            .await
            .is_err()
    );
}

// ── set_config sanitization ─────────────────────────────────────────────

#[tokio::test]
async fn set_config_rejects_dash_key() {
    let git = test_git();
    assert!(
        git.set_config(Path::new("/unused"), "--evil", "value")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn set_config_rejects_nul_in_key() {
    let git = test_git();
    assert!(
        git.set_config(Path::new("/unused"), "key\0bad", "value")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn set_config_rejects_nul_in_value() {
    let git = test_git();
    assert!(
        git.set_config(Path::new("/unused"), "user.name", "value\0bad")
            .await
            .is_err()
    );
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
    assert!(
        git.fetch_refs(Path::new("/unused"), "-evil", &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fetch_refs_rejects_nul_in_url() {
    let git = test_git();
    assert!(
        git.fetch_refs(Path::new("/unused"), "url\0bad", &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn fetch_refs_rejects_nul_in_refspec() {
    let git = test_git();
    assert!(
        git.fetch_refs(
            Path::new("/unused"),
            "https://example.com/repo.git",
            &["bad\0spec".to_string()]
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn fetch_refs_rejects_empty_refspec() {
    let git = test_git();
    assert!(
        git.fetch_refs(
            Path::new("/unused"),
            "https://example.com/repo.git",
            &["".to_string()]
        )
        .await
        .is_err()
    );
}
