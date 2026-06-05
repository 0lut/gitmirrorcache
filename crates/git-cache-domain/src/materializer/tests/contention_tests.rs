use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_materialize_same_branch() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Arc::new(Materializer::new(state));

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let m = Arc::clone(&materializer);
            let repo = fixture.repo.clone();
            tokio::spawn(async move {
                m.materialize(MaterializeRequest {
                    repo,
                    selector: Selector::Branch(BranchName::parse("main").unwrap()),
                    mode: RequestMode::Strict,
                    upstream_authorization: Default::default(),
                })
                .await
            })
        })
        .collect();

    let mut commits = Vec::new();
    for handle in handles {
        let result = handle.await.unwrap();
        if let Ok(response) = result {
            commits.push(response.commit);
        }
    }

    assert!(!commits.is_empty(), "at least one materialize must succeed");
    let first = &commits[0];
    for c in &commits {
        assert_eq!(
            c, first,
            "all successful materializations return same commit"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_session_creation_unique_ids() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Arc::new(Materializer::new(state));

    // First materialize to ensure commit is available.
    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let commit = response.commit;

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let m = Arc::clone(&materializer);
            let repo = fixture.repo.clone();
            let c = commit.clone();
            tokio::spawn(async move {
                m.create_session(repo, c, MaterializeSource::CacheVerified)
                    .await
            })
        })
        .collect();

    let mut session_refs = Vec::new();
    for handle in handles {
        let response = handle.await.unwrap().unwrap();
        session_refs.push(response.ref_name);
    }

    assert_eq!(session_refs.len(), 10);
    // All session IDs must be unique.
    let unique: std::collections::HashSet<_> = session_refs.iter().collect();
    assert_eq!(unique.len(), 10, "all session IDs should be unique");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cleanup_no_double_delete() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Arc::new(Materializer::new(Arc::clone(&state)));

    // Create some sessions to clean up.
    for _ in 0..5 {
        let _ = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
                upstream_authorization: Default::default(),
            })
            .await;
    }

    // Spawn 3 concurrent cleanup tasks.
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let m = Arc::clone(&materializer);
            tokio::spawn(async move { m.cleanup_expired_sessions().await })
        })
        .collect();

    for handle in handles {
        // Should not panic or return an error from double-delete.
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "cleanup should succeed: {:?}", result.err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn materialize_during_upstream_change() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Arc::new(Materializer::new(state));

    // Capture the original commit BEFORE pushing a new one.
    let original_commit = fixture.head_commit();

    // Start a materialize.
    let m1 = Arc::clone(&materializer);
    let repo1 = fixture.repo.clone();
    let first_handle = tokio::spawn(async move {
        m1.materialize(MaterializeRequest {
            repo: repo1,
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
    });

    // Push a new commit upstream while the first materialize is running.
    let new_commit = fixture.commit_and_push("mid-flight change");

    let first_result = first_handle.await.unwrap();
    // First should succeed with either the old or new commit.
    match first_result {
        Ok(resp) => {
            assert!(
                resp.commit == original_commit || resp.commit == new_commit,
                "should return a valid commit"
            );
        }
        Err(_) => {
            // Conflict during fetch is acceptable (branch moved).
        }
    }

    // Second materialize should see the new commit.
    let resp = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(resp.commit, new_commit);
}
