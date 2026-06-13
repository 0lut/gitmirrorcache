use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

// ── parse_git_version tests ─────────────────────────────────────

#[test]
fn parse_git_version_plain() {
    assert_eq!(parse_git_version("git version 2.47.3\n"), Some((2, 47, 3)));
    assert_eq!(parse_git_version("git version 2.39.5"), Some((2, 39, 5)));
}

#[test]
fn parse_git_version_with_platform_suffix() {
    assert_eq!(
        parse_git_version("git version 2.39.5 (Apple Git-154)\n"),
        Some((2, 39, 5))
    );
    assert_eq!(
        parse_git_version("git version 2.47.0.windows.1"),
        Some((2, 47, 0))
    );
    assert_eq!(
        parse_git_version("git version 2.46.0-rc1"),
        Some((2, 46, 0))
    );
}

#[test]
fn parse_git_version_missing_patch() {
    assert_eq!(parse_git_version("git version 2.45"), Some((2, 45, 0)));
}

#[test]
fn parse_git_version_rejects_garbage() {
    assert_eq!(parse_git_version(""), None);
    assert_eq!(parse_git_version("not git"), None);
    assert_eq!(parse_git_version("git version two.three"), None);
}

// ── reject_ref_arg tests ────────────────────────────────────────

#[test]
fn reject_ref_arg_rejects_empty() {
    assert!(reject_ref_arg("", "ref").is_err());
}

#[test]
fn reject_ref_arg_rejects_leading_dash() {
    assert!(reject_ref_arg("-evil", "ref").is_err());
    assert!(reject_ref_arg("--flag", "ref").is_err());
}

#[test]
fn reject_ref_arg_rejects_colon() {
    assert!(reject_ref_arg("HEAD:path", "ref").is_err());
}

#[test]
fn reject_ref_arg_rejects_nul() {
    assert!(reject_ref_arg("ref\0name", "ref").is_err());
}

#[test]
fn reject_ref_arg_accepts_valid() {
    assert!(reject_ref_arg("refs/heads/main", "ref").is_ok());
    assert!(reject_ref_arg("feature/test", "ref").is_ok());
}

// ── reject_revision_arg tests ───────────────────────────────────

#[test]
fn reject_revision_arg_rejects_empty() {
    assert!(reject_revision_arg("").is_err());
}

#[test]
fn reject_revision_arg_rejects_leading_dash() {
    assert!(reject_revision_arg("-evil").is_err());
}

#[test]
fn reject_revision_arg_rejects_nul() {
    assert!(reject_revision_arg("rev\0ision").is_err());
}

#[test]
fn reject_revision_arg_allows_colon() {
    assert!(reject_revision_arg("HEAD:path").is_ok());
}

#[test]
fn reject_revision_arg_accepts_valid() {
    assert!(reject_revision_arg("abc123").is_ok());
    assert!(reject_revision_arg("HEAD^{commit}").is_ok());
}

// ── reject_config_key tests ─────────────────────────────────────

#[test]
fn reject_config_key_rejects_empty() {
    assert!(reject_config_key("").is_err());
}

#[test]
fn reject_config_key_rejects_leading_dash() {
    assert!(reject_config_key("-bad").is_err());
}

#[test]
fn reject_config_key_rejects_nul() {
    assert!(reject_config_key("key\0val").is_err());
}

#[test]
fn reject_config_key_allows_equals() {
    assert!(reject_config_key("key=value").is_ok());
}

#[test]
fn upstream_error_mapping_preserves_timeout() {
    let git = Git::default_with_timeout(Duration::from_secs(1));

    let error = git.map_upstream_git_error(GitCacheError::Timeout("slow git".into()));

    assert!(
        matches!(&error, GitCacheError::Timeout(message) if message == "slow git"),
        "timeout should not be remapped to upstream unavailable: {error}"
    );
}

#[tokio::test]
async fn upstream_auth_env_appends_to_existing_git_config_entries() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "git-cache-auth-env-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("fake-git");
    let env_out = root.join("env.txt");
    std::fs::write(&script, "#!/bin/sh\nenv > \"$FAKE_ENV_OUT\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }

    let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();
    let git = Git::new(&script, Duration::from_secs(5))
        .with_env("FAKE_ENV_OUT", env_out.as_os_str().to_os_string())
        .with_env("GIT_CONFIG_COUNT", "1")
        .with_env("GIT_CONFIG_KEY_0", "http.https://example.com/.extraHeader")
        .with_env("GIT_CONFIG_VALUE_0", "Authorization: Bearer process")
        .with_upstream_auth("https://github.com/org/repo.git", &auth)
        .unwrap();

    git.ls_remote_heads("https://github.com/org/repo.git")
        .await
        .unwrap();

    let env = std::fs::read_to_string(&env_out).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(env.contains("GIT_CONFIG_COUNT=2"), "{env}");
    assert!(
        env.contains("GIT_CONFIG_KEY_0=http.https://example.com/.extraHeader"),
        "{env}"
    );
    assert!(
        env.contains("GIT_CONFIG_VALUE_0=Authorization: Bearer process"),
        "{env}"
    );
    assert!(
        env.contains("GIT_CONFIG_KEY_1=http.https://github.com/.extraHeader"),
        "{env}"
    );
    assert!(
        env.contains("GIT_CONFIG_VALUE_1=Authorization: Basic dXNlcjpwYXNz"),
        "{env}"
    );
}

#[tokio::test]
async fn upstream_auth_env_replaces_existing_entry_for_same_host() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "git-cache-auth-env-same-host-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("fake-git");
    let env_out = root.join("env.txt");
    std::fs::write(&script, "#!/bin/sh\nenv > \"$FAKE_ENV_OUT\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }

    let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();
    let git = Git::new(&script, Duration::from_secs(5))
        .with_env("FAKE_ENV_OUT", env_out.as_os_str().to_os_string())
        .with_env("GIT_CONFIG_COUNT", "1")
        .with_env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraHeader")
        .with_env("GIT_CONFIG_VALUE_0", "Authorization: Bearer process")
        .with_upstream_auth("https://github.com/org/repo.git", &auth)
        .unwrap();

    git.ls_remote_heads("https://github.com/org/repo.git")
        .await
        .unwrap();

    let env = std::fs::read_to_string(&env_out).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(env.contains("GIT_CONFIG_COUNT=1"), "{env}");
    assert!(
        env.contains("GIT_CONFIG_KEY_0=http.https://github.com/.extraHeader"),
        "{env}"
    );
    assert!(
        env.contains("GIT_CONFIG_VALUE_0=Authorization: Basic dXNlcjpwYXNz"),
        "{env}"
    );
    assert!(!env.contains("Authorization: Bearer process"), "{env}");
}

// ── reject_remote_url tests ─────────────────────────────────────

#[test]
fn reject_remote_url_rejects_empty() {
    assert!(reject_remote_url("").is_err());
}

#[test]
fn reject_remote_url_rejects_leading_dash() {
    assert!(reject_remote_url("-evil").is_err());
}

#[test]
fn reject_remote_url_rejects_nul() {
    assert!(reject_remote_url("url\0bad").is_err());
}

#[test]
fn reject_remote_url_accepts_valid() {
    assert!(reject_remote_url("https://github.com/org/repo.git").is_ok());
    assert!(reject_remote_url("/path/to/repo").is_ok());
}

// ── reject_refspec tests ────────────────────────────────────────

#[test]
fn reject_refspec_rejects_empty() {
    assert!(reject_refspec("").is_err());
}

#[test]
fn reject_refspec_rejects_nul() {
    assert!(reject_refspec("spec\0bad").is_err());
}

#[test]
fn reject_refspec_allows_leading_plus() {
    assert!(reject_refspec("+refs/heads/main:refs/heads/main").is_ok());
}

#[test]
fn reject_refspec_allows_colon() {
    assert!(reject_refspec("refs/heads/main:refs/remotes/origin/main").is_ok());
}

// ── reject_fetch_filter tests ───────────────────────────────────

#[test]
fn reject_fetch_filter_allows_blob_none() {
    assert!(reject_fetch_filter("blob:none").is_ok());
}

#[test]
fn reject_fetch_filter_rejects_other_values() {
    assert!(reject_fetch_filter("").is_err());
    assert!(reject_fetch_filter("--upload-pack=evil").is_err());
    assert!(reject_fetch_filter("blob:limit=10").is_err());
    assert!(reject_fetch_filter("blob:none\0bad").is_err());
}

#[test]
fn reject_fetch_depth_rejects_zero() {
    assert!(reject_fetch_depth(0).is_err());
    assert!(reject_fetch_depth(1).is_ok());
}

// ── reject_nul tests ────────────────────────────────────────────

#[test]
fn reject_nul_rejects_nul_byte() {
    assert!(reject_nul("hello\0world", "value").is_err());
}

#[test]
fn reject_nul_accepts_clean_string() {
    assert!(reject_nul("hello world", "value").is_ok());
}

#[tokio::test]
async fn fetch_ref_passes_depth_and_filter_before_remote() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "git-cache-fetch-ref-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("fake-git");
    let args_out = root.join("args.txt");
    let repo_dir = root.join("repo.git");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        &script,
        r#"#!/bin/sh
for arg in "$@"; do
  printf '[%s]' "$arg" >> "$FAKE_ARGS_OUT"
done
printf '\n' >> "$FAKE_ARGS_OUT"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }

    let git = Git::new(&script, Duration::from_secs(5))
        .with_env("FAKE_ARGS_OUT", args_out.as_os_str().to_os_string());
    git.fetch_ref(
        &repo_dir,
        "https://github.com/org/repo.git",
        "refs/heads/main",
        "refs/cache/upstream/heads/main",
        FetchOptions {
            filter: Some("blob:none"),
            depth: Some(1),
            refetch: false,
            unshallow: false,
        },
    )
    .await
    .unwrap();

    let args = std::fs::read_to_string(&args_out).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        args.lines().any(|line| line
            == "[fetch][--no-tags][--depth=1][--filter=blob:none][--][https://github.com/org/repo.git][+refs/heads/main:refs/cache/upstream/heads/main]"),
        "{args}"
    );
}

#[tokio::test]
async fn unfiltered_fetch_clears_persisted_partial_clone_filter() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "git-cache-fetch-nofilter-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("fake-git");
    let args_out = root.join("args.txt");
    let repo_dir = root.join("repo.git");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        &script,
        r#"#!/bin/sh
for arg in "$@"; do
  printf '[%s]' "$arg" >> "$FAKE_ARGS_OUT"
done
printf '\n' >> "$FAKE_ARGS_OUT"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
    }

    let git = Git::new(&script, Duration::from_secs(5))
        .with_env("FAKE_ARGS_OUT", args_out.as_os_str().to_os_string());
    git.fetch_ref(
        &repo_dir,
        "https://github.com/org/repo.git",
        "refs/heads/main",
        "refs/cache/upstream/heads/main",
        FetchOptions {
            filter: None,
            depth: None,
            refetch: true,
            unshallow: false,
        },
    )
    .await
    .unwrap();

    let args = std::fs::read_to_string(&args_out).unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        args.lines().any(|line| line
            == "[-c][remote.https://github.com/org/repo.git.partialclonefilter=][fetch][--no-tags][--refetch][--][https://github.com/org/repo.git][+refs/heads/main:refs/cache/upstream/heads/main]"),
        "{args}"
    );
}

#[test]
fn filtered_fetch_args_do_not_clear_partial_clone_filter() {
    let args = fetch_args_with_options(
        FetchOptions {
            filter: Some("blob:none"),
            depth: Some(1),
            refetch: false,
            unshallow: false,
        },
        "https://github.com/org/repo.git",
    )
    .unwrap();
    assert_eq!(args[0], OsString::from("fetch"));
    assert!(!args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("partialclonefilter")));
}

// ── Public method rejection of dash-prefixed arguments ──────────

fn test_git() -> Git {
    Git::default_with_timeout(Duration::from_secs(1))
}

#[tokio::test]
async fn rev_parse_rejects_dash_rev() {
    let git = test_git();
    assert!(git.rev_parse(Path::new("/unused"), "--evil").await.is_err());
}

#[tokio::test]
async fn for_each_ref_commits_rejects_dash_ref_prefix() {
    let git = test_git();
    assert!(git
        .for_each_ref_commits(Path::new("/unused"), "-evil")
        .await
        .is_err());
}

#[tokio::test]
async fn for_each_ref_rejects_dash_ref_prefix() {
    let git = test_git();
    assert!(git
        .for_each_ref(Path::new("/unused"), "-evil")
        .await
        .is_err());
}

#[tokio::test]
async fn for_each_ref_containing_commit_rejects_dash_ref_prefix() {
    let git = test_git();
    let commit = CommitSha::parse("a".repeat(40)).unwrap();
    assert!(git
        .for_each_ref_containing_commit(Path::new("/unused"), &commit, &["-evil"])
        .await
        .is_err());
}

#[tokio::test]
async fn update_ref_rejects_dash_ref_name() {
    let git = test_git();
    assert!(git
        .update_ref(Path::new("/unused"), "-evil", "abc123")
        .await
        .is_err());
}

#[tokio::test]
async fn delete_ref_rejects_dash_ref_name() {
    let git = test_git();
    assert!(git.delete_ref(Path::new("/unused"), "-evil").await.is_err());
}

#[tokio::test]
async fn symbolic_ref_rejects_dash_name() {
    let git = test_git();
    assert!(git
        .symbolic_ref(Path::new("/unused"), "--evil", "refs/heads/main")
        .await
        .is_err());
}

#[tokio::test]
async fn set_config_rejects_dash_key() {
    let git = test_git();
    assert!(git
        .set_config(Path::new("/unused"), "--evil", "value")
        .await
        .is_err());
}

#[tokio::test]
async fn ls_remote_heads_rejects_dash_url() {
    let git = test_git();
    assert!(git.ls_remote_heads("-evil").await.is_err());
}

#[tokio::test]
async fn ls_remote_default_branch_rejects_dash_url() {
    let git = test_git();
    assert!(git.ls_remote_default_branch("-evil").await.is_err());
}

#[tokio::test]
async fn fetch_ref_rejects_dash_url() {
    let git = test_git();
    assert!(git
        .fetch_ref(
            Path::new("/unused"),
            "-evil",
            "refs/heads/main",
            "refs/cache/upstream/heads/main",
            FetchOptions::default(),
        )
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_ref_rejects_dash_upstream_ref() {
    let git = test_git();
    assert!(git
        .fetch_ref(
            Path::new("/unused"),
            "https://example.com/repo.git",
            "-evil",
            "refs/cache/upstream/heads/main",
            FetchOptions::default(),
        )
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_ref_rejects_dash_local_ref() {
    let git = test_git();
    assert!(git
        .fetch_ref(
            Path::new("/unused"),
            "https://example.com/repo.git",
            "refs/heads/main",
            "-evil",
            FetchOptions::default(),
        )
        .await
        .is_err());
}

#[tokio::test]
async fn fetch_objects_rejects_zero_depth() {
    let git = test_git();
    let commit = CommitSha::parse("a".repeat(40)).unwrap();
    assert!(git
        .fetch_objects(
            Path::new("/unused"),
            "https://example.com/repo.git",
            std::slice::from_ref(&commit),
            FetchOptions {
                filter: None,
                depth: Some(0),
                refetch: false,
                unshallow: false,
            },
        )
        .await
        .is_err());
}
