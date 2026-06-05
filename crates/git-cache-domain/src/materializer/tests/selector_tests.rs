use super::*;

#[tokio::test]
async fn cached_exact_commit_survives_upstream_offline() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let branch_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(branch_response.source, MaterializeSource::UpstreamVerified);
    let commit_manifest =
        wait_for_commit_manifest(&materializer.state, &fixture.repo, &branch_response.commit).await;
    wait_for_verified_generation(
        &materializer.state,
        &fixture.repo,
        commit_manifest.generation,
    )
    .await;

    stdfs::remove_dir_all(fixture.cache_root().join("repos")).unwrap();
    stdfs::rename(
        fixture.upstream_path(),
        fixture.tmp.path().join("offline.git"),
    )
    .unwrap();

    let commit_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(branch_response.commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(commit_response.source, MaterializeSource::CacheVerified);
    assert_eq!(commit_response.commit, branch_response.commit);
}

#[tokio::test]
async fn short_commit_selector_resolves_to_full_commit_from_upstream() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));
    let head = fixture.head_commit();
    let short = ShortCommitSha::parse(&head.as_str()[..8]).unwrap();

    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::ShortCommit(short),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(response.source, MaterializeSource::UpstreamVerified);
    assert_eq!(response.commit, head);
    let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &head).await;
}

#[tokio::test]
async fn short_commit_selector_revalidates_even_when_commit_is_cached() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let branch_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let short = ShortCommitSha::parse(&branch_response.commit.as_str()[..8]).unwrap();

    let short_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::ShortCommit(short),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(short_response.source, MaterializeSource::UpstreamVerified);
    assert_eq!(short_response.commit, branch_response.commit);
}

#[tokio::test]
async fn short_commit_selector_requires_upstream_even_when_cached() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let branch_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let short = ShortCommitSha::parse(&branch_response.commit.as_str()[..8]).unwrap();

    stdfs::rename(
        fixture.upstream_path(),
        fixture.tmp.path().join("offline.git"),
    )
    .unwrap();

    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::ShortCommit(short),
            mode: RequestMode::Cached,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));
}

#[tokio::test]
async fn unknown_short_commit_returns_not_found_after_upstream_check() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::ShortCommit(ShortCommitSha::parse("deadbeef").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::NotFound(_)));
}

#[tokio::test]
async fn branch_and_default_selectors_require_upstream_for_all_modes() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let default_response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::DefaultBranch,
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(default_response.source, MaterializeSource::UpstreamVerified);

    stdfs::rename(
        fixture.upstream_path(),
        fixture.tmp.path().join("offline.git"),
    )
    .unwrap();
    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));

    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Cached,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));

    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::DefaultBranch,
            mode: RequestMode::Cached,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::UpstreamUnavailable(_)));
}

#[tokio::test]
async fn force_push_updates_branch_manifest_without_removing_old_commit() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    let second_commit = fixture.commit_and_push("second");
    let second = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(second.commit, second_commit);
    assert_ne!(first.commit, second.commit);
    let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &first.commit).await;
    let _ = wait_for_commit_manifest(&materializer.state, &fixture.repo, &second.commit).await;
}

#[tokio::test]
async fn short_commit_selector_rejects_unreachable_stale_local_commit() {
    let fixture = GitFixture::new();
    let state = fixture.state();
    let materializer = Materializer::new(Arc::new(state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let replacement = fixture.replace_history_and_push("replacement");
    let stale_short = short_prefix_not_matching(&first.commit, &replacement);

    let error = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::ShortCommit(stale_short),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap_err();

    assert!(matches!(error, GitCacheError::NotFound(_)));
}
