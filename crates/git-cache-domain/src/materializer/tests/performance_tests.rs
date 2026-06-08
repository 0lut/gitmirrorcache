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

// ── synthesize_ref_advertisement unit tests ─────────────────────────
