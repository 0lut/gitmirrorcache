use super::*;

#[test]
fn protected_session_token_uses_32_random_bytes_of_hex() {
    let token = new_session_token();
    let random_hex = token
        .strip_prefix("gcs_")
        .expect("token should include gcs_ prefix");

    assert_eq!(random_hex.len(), 64);
    assert!(
        random_hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "token suffix should be lowercase hex"
    );

    let tokens: HashSet<_> = (0..128).map(|_| new_session_token()).collect();
    assert_eq!(tokens.len(), 128);
}

#[tokio::test]
async fn create_protected_session_rejects_commit_without_tree_object() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));
    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    let commit_body = b"tree 0000000000000000000000000000000000000000\nauthor Cache Test <cache@example.invalid> 0 +0000\ncommitter Cache Test <cache@example.invalid> 0 +0000\n\nmissing tree\n";
    let mut child = Command::new("git")
        .current_dir(&repo_dir)
        .args(["hash-object", "-w", "-t", "commit", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(commit_body)
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "git hash-object failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let commit = CommitSha::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert!(materializer.commit_exists(&repo_dir, &commit).await);
    assert!(
        !materializer
            .commit_ready_for_serving(&repo_dir, &commit)
            .await
    );

    let result = materializer
        .create_protected_session(
            fixture.repo.clone(),
            commit,
            MaterializeSource::UpstreamAuthorizedCacheHit,
            vec!["refs/heads/main".into()],
        )
        .await;

    assert!(matches!(result, Err(GitCacheError::NotFound(_))));
}

#[tokio::test]
async fn session_advertises_only_upload_pack_refs() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let session_id = SessionId::parse(
        response
            .ref_name
            .strip_prefix("refs/cache/sessions/")
            .unwrap(),
    )
    .unwrap();
    let session_repo = materializer
        .session_repo_from_manifest(&fixture.repo, session_id, None)
        .await
        .unwrap();
    let advertised = advertise_refs(&state, &session_repo).await.unwrap();
    let advertised = String::from_utf8_lossy(&advertised);

    assert!(advertised.contains(&response.ref_name));
    assert!(advertised.contains(" filter "));
    assert!(!advertised.contains("git-receive-pack"));
}

// ── Contention Tests ─────────────────────────────────────────────────
