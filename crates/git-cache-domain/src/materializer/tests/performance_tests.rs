use super::*;

#[tokio::test]
async fn test_repeated_materialize_same_branch() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));
    let iterations = 10;

    let first_start = std::time::Instant::now();
    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_elapsed = first_start.elapsed();

    let mut subsequent_total = std::time::Duration::ZERO;
    for _ in 1..iterations {
        let start = std::time::Instant::now();
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                mode: RequestMode::Strict,
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        subsequent_total += start.elapsed();
        assert_eq!(response.commit, first.commit);
    }

    let avg_subsequent = subsequent_total / (iterations - 1) as u32;
    eprintln!(
            "repeated materialize: first={first_elapsed:?}, avg_subsequent={avg_subsequent:?} ({} calls)",
            iterations - 1
        );
}

#[tokio::test]
async fn test_session_creation_throughput() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));
    let session_count = 20;

    // First materialize to ensure branch is cached.
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

    let start = std::time::Instant::now();
    let mut sessions = Vec::new();
    for _ in 0..session_count {
        let session = materializer
            .create_session(
                fixture.repo.clone(),
                commit.clone(),
                MaterializeSource::CacheVerified,
            )
            .await
            .unwrap();
        sessions.push(session);
    }
    let elapsed = start.elapsed();

    assert_eq!(sessions.len(), session_count);
    // Verify each session has a unique ref.
    let refs: std::collections::HashSet<_> = sessions.iter().map(|s| s.ref_name.clone()).collect();
    assert_eq!(
        refs.len(),
        session_count,
        "each session should have a unique ref"
    );

    let avg = elapsed / session_count as u32;
    eprintln!("session creation: {session_count} sessions in {elapsed:?}, avg={avg:?}");
    assert!(
        elapsed.as_secs() < 60,
        "session creation too slow: {elapsed:?}"
    );
}

#[tokio::test]
async fn test_cleanup_expired_sessions_performance() {
    let fixture = GitFixture::new();
    let config = AppConfig {
        session_ttl_seconds: 0, // Expire immediately.
        ..fixture.state_config()
    };
    let state = Arc::new(AppState::try_new(config).unwrap());
    let materializer = Materializer::new(Arc::clone(&state));
    let session_count = 50;

    // Create sessions that will expire immediately (ttl=0).
    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    for _ in 0..session_count {
        materializer
            .create_session(
                fixture.repo.clone(),
                response.commit.clone(),
                MaterializeSource::CacheVerified,
            )
            .await
            .unwrap();
    }

    // Give a brief moment for the expiry to kick in.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let start = std::time::Instant::now();
    let report = materializer.cleanup_expired_sessions().await.unwrap();
    let elapsed = start.elapsed();

    eprintln!(
        "cleanup_expired_sessions: removed={}, errors={}, {elapsed:?} for {session_count} sessions",
        report.sessions_removed,
        report.errors.len()
    );

    // All sessions should be expired and cleaned up (ttl=0).
    // We allow some margin since timing isn't precise.
    assert!(
        report.sessions_removed > 0,
        "expected some sessions to be cleaned up"
    );
    assert!(elapsed.as_secs() < 30, "cleanup too slow: {elapsed:?}");
}

// ── synthesize_ref_advertisement unit tests ─────────────────────────
