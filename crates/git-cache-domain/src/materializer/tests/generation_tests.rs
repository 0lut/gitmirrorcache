use super::*;

#[tokio::test]
async fn publish_generation_links_delta_to_previous_generation() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let first_generation =
        generation_manifest_for(&state, &fixture.repo, first_manifest.generation).await;
    assert_eq!(first_generation.parent_generation, None);

    let second_commit = fixture.commit_and_push("second");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
    let second_generation =
        generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
    assert_eq!(
        second_generation.parent_generation,
        Some(first_manifest.generation)
    );

    let head = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
    assert_eq!(head.generation, second_manifest.generation);
    assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
}

#[tokio::test]
async fn pending_generation_verifies_from_local_repo_without_parent_bundle() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let parent_head =
        wait_for_generation_head(&state, &fixture.repo, first_manifest.generation).await;
    state
        .store
        .delete(&bundle_key(&fixture.repo, first_manifest.generation))
        .await
        .unwrap();

    let child_commit = fixture.commit_and_push("second");
    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    run_git(
        &repo_dir,
        [
            "fetch",
            "--no-tags",
            fixture.upstream_path().to_str().unwrap(),
            "+refs/heads/main:refs/cache/upstream/heads/main",
        ],
    );

    let child_generation = GenerationId::new();
    let reservation = state.disk.reserve(1024 * 1024 * 64).await.unwrap();
    let temp_path = reservation.temp_path().unwrap();
    fs::create_dir_all(&temp_path).await.unwrap();
    let bundle_path = temp_path.join("pending-child.bundle");
    state
        .git
        .bundle_create_incremental(&repo_dir, &bundle_path, &parent_head.tip_commits)
        .await
        .unwrap();

    let now = Utc::now();
    let child_manifest = GenerationManifest {
        repo: fixture.repo.clone(),
        generation: child_generation,
        bundle_key: bundle_key(&fixture.repo, child_generation),
        parent_generation: Some(parent_head.generation),
        created_at: now,
        commits: vec![child_commit.clone()],
    };
    let mut child_tip_commits = parent_head.tip_commits.clone();
    push_unique_commit(&mut child_tip_commits, child_commit.clone());
    let child_head = RepoGenerationHead {
        repo: fixture.repo.clone(),
        generation: child_generation,
        tip_commits: child_tip_commits,
        updated_at: now,
    };
    let child_manifests = PublishManifests {
        commits: vec![CommitManifest {
            repo: fixture.repo.clone(),
            commit: child_commit.clone(),
            generation: child_generation,
            complete: true,
            verified_at: now,
        }],
        refs: Vec::new(),
        sessions: Vec::new(),
    };
    GenerationPublish::with_manifests(child_manifest, child_manifests)
        .publish_pending_bundle_file(&*state.store, &bundle_path, child_head, None)
        .await
        .unwrap();
    reservation.release().await.unwrap();

    materializer
        .verify_generation_with_semaphore(fixture.repo.clone(), child_generation, false)
        .await
        .unwrap();
    let child_manifest = wait_for_commit_manifest(&state, &fixture.repo, &child_commit).await;
    assert_eq!(child_manifest.generation, child_generation);
}

#[tokio::test]
async fn hydrate_commit_restores_parent_generation_chain_from_cold_cache() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

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
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let _ = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;

    let repo_dir = materializer.repo_dir(&fixture.repo);
    stdfs::remove_dir_all(&repo_dir).unwrap();
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(second_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    materializer
        .state
        .git
        .rev_parse(&repo_dir, &format!("{}^{{commit}}", first.commit.as_str()))
        .await
        .unwrap();
    assert!(materializer.commit_exists(&repo_dir, &second_commit).await);
}

#[tokio::test]
async fn exact_ancestor_in_known_generation_indexes_without_new_bundle() {
    let fixture = GitFixture::new();
    let ancestor_commit = fixture.head_commit();
    let tip_commit = fixture.commit_and_push("second");
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
            .await
            .unwrap()
            .is_none()
    );
    let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
    wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;
    let head_before =
        Some(wait_for_generation_head(&state, &fixture.repo, tip_manifest.generation).await);
    let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(ancestor_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(response.source, MaterializeSource::CacheVerified);
    assert_eq!(response.commit, ancestor_commit);

    let ancestor_manifest = read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ancestor_manifest.generation, tip_manifest.generation);
    let head_after = read_repo_generation_head(&*state.store, &fixture.repo)
        .await
        .unwrap();
    let generation_keys_after = generation_object_keys(&state, &fixture.repo).await;
    assert_eq!(head_after, head_before);
    assert_eq!(generation_keys_after, generation_keys_before);
}

#[tokio::test]
async fn exact_ancestor_uses_local_cache_refs_when_generation_head_is_stale() {
    let fixture = GitFixture::new();
    let ancestor_commit = fixture.commit_and_push("second");
    let tip_commit = fixture.commit_and_push("third");
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
            .await
            .unwrap()
            .is_none()
    );
    let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
    wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;
    let stale_head = RepoGenerationHead {
        repo: fixture.repo.clone(),
        generation: tip_manifest.generation,
        tip_commits: vec![ancestor_commit.clone()],
        updated_at: Utc::now(),
    };
    write_repo_generation_head(&*state.store, &stale_head)
        .await
        .unwrap();
    let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(ancestor_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(response.source, MaterializeSource::CacheVerified);
    assert_eq!(response.commit, ancestor_commit);

    let ancestor_manifest = read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ancestor_manifest.generation, tip_manifest.generation);
    let generation_keys_after = generation_object_keys(&state, &fixture.repo).await;
    assert_eq!(generation_keys_after, generation_keys_before);
}

#[tokio::test]
async fn exact_descendants_after_cold_ancestor_fetch_reuse_full_bundle() {
    let fixture = GitFixture::new();
    let tip_2 = fixture.head_commit();
    let tip_1 = fixture.commit_and_push("second");
    let tip = fixture.commit_and_push("third");
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(tip_2.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(first.commit, tip_2);
    assert_eq!(first.source, MaterializeSource::UpstreamVerified);
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_2).await;
    wait_for_verified_generation(&state, &fixture.repo, first_manifest.generation).await;
    let generation_keys_after_first = generation_object_keys(&state, &fixture.repo).await;

    let second = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(tip_1.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(second.commit, tip_1);
    assert_eq!(second.source, MaterializeSource::CacheVerified);
    assert_eq!(
        generation_object_keys(&state, &fixture.repo).await,
        generation_keys_after_first
    );

    let third = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(tip.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(third.commit, tip);
    assert_eq!(third.source, MaterializeSource::CacheVerified);
    assert_eq!(
        generation_object_keys(&state, &fixture.repo).await,
        generation_keys_after_first
    );
}

#[tokio::test]
async fn exact_commit_ahead_of_known_generation_publishes_incremental_bundle() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let second_commit = fixture.commit_and_push("second");

    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(second_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(response.commit, second_commit);
    assert_eq!(response.source, MaterializeSource::UpstreamVerified);

    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
    let second_generation =
        generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
    assert_eq!(
        second_generation.parent_generation,
        Some(first_manifest.generation)
    );
    let head = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
    assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
}

#[tokio::test]
async fn delta_publish_falls_back_to_full_bundle_when_previous_tip_is_missing_locally() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    stdfs::remove_dir_all(materializer.repo_dir(&fixture.repo)).unwrap();
    let second_commit = fixture.replace_history_and_push("replacement");
    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    run_git(
        &repo_dir,
        [
            "fetch",
            "--no-tags",
            fixture.upstream_path().to_str().unwrap(),
            "+refs/heads/main:refs/cache/upstream/heads/main",
        ],
    );
    let response = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(second_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(response.commit, second_commit);

    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
    let second_generation =
        generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
    assert_ne!(first_manifest.generation, second_manifest.generation);
    assert_eq!(second_generation.parent_generation, None);
    let head = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
    assert_eq!(head.tip_commits, vec![second_commit]);
}

#[tokio::test]
async fn ensure_repo_dir_records_disk_metadata() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    assert!(repo_dir.join("config").exists());

    let index = state.disk.repo_index().await.unwrap();
    let repo_path = PathBuf::from(fixture.repo.local_bare_path());
    let entry = index.repos.get(&repo_path).unwrap();
    assert_eq!(entry.path, repo_path);
    assert!(entry.size_bytes > 0);
}

#[tokio::test]
async fn ensure_repo_dir_invalidates_partial_repo_cache() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));
    let repo_path = PathBuf::from(fixture.repo.local_bare_path());
    let repo_dir = materializer.repo_dir(&fixture.repo);
    stdfs::create_dir_all(&repo_dir).unwrap();
    stdfs::write(repo_dir.join("partial"), "stale").unwrap();
    state
        .disk
        .record_repo_access(repo_path.clone())
        .await
        .unwrap();

    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

    assert!(repo_dir.join("config").exists());
    assert!(!repo_dir.join("partial").exists());
    assert!(state
        .disk
        .repo_index()
        .await
        .unwrap()
        .repos
        .contains_key(&repo_path));
}

#[tokio::test]
async fn compact_generation_chain_replaces_long_chain_with_single_root() {
    let fixture = GitFixture::new();
    let config = AppConfig {
        compaction: git_cache_core::CompactionConfig {
            chain_depth_threshold: 2,
            inline: false,
        },
        ..fixture.state_config()
    };
    let state = Arc::new(AppState::try_new(config).unwrap());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let second_commit = fixture.commit_and_push("second");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
    let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
    let third_commit = fixture.commit_and_push("third");
    fixture.push_head_to_branch("default");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third_commit).await;
    let _ = wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("default").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();

    let report = materializer
        .compact_generation_chain(&fixture.repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.old_chain_depth, 3);

    let head = read_repo_generation_head(&*state.store, &fixture.repo)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(head.generation, report.new_generation);
    let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
    assert_eq!(compacted.parent_generation, None);

    for old_generation in &report.old_generations {
        assert!(
            read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                .await
                .unwrap()
                .is_none()
        );
        assert!(state
            .store
            .get(&bundle_key(&fixture.repo, *old_generation))
            .await
            .unwrap()
            .is_none());
        assert!(
            read_verified_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                .await
                .unwrap()
                .is_none()
        );
    }

    for commit in [
        first.commit.clone(),
        second_commit.clone(),
        third_commit.clone(),
    ] {
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &commit).await;
        assert_eq!(manifest.generation, report.new_generation);
    }
    let branch_manifest = read_ref_manifest(
        &*state.store,
        &fixture.repo,
        &BranchName::parse("main").unwrap().ref_name(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(branch_manifest.generation, report.new_generation);
    let default_branch_manifest = read_ref_manifest(
        &*state.store,
        &fixture.repo,
        &BranchName::parse("default").unwrap().ref_name(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(default_branch_manifest.generation, report.new_generation);
    assert_ne!(first_manifest.generation, report.new_generation);

    let repo_dir = materializer.repo_dir(&fixture.repo);
    stdfs::remove_dir_all(&repo_dir).unwrap();
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Commit(third_commit.clone()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    for commit in [first.commit, second_commit, third_commit] {
        materializer
            .state
            .git
            .rev_parse(&repo_dir, &format!("{}^{{commit}}", commit.as_str()))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn inline_compaction_runs_after_verified_head_update() {
    let fixture = GitFixture::new();
    let config = AppConfig {
        compaction: git_cache_core::CompactionConfig {
            chain_depth_threshold: 2,
            inline: true,
        },
        ..fixture.state_config()
    };
    let state = Arc::new(AppState::try_new(config).unwrap());
    let materializer = Materializer::new(Arc::clone(&state));

    let first = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let first_manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
    let _ = wait_for_generation_head(&state, &fixture.repo, first_manifest.generation).await;

    let second = fixture.commit_and_push("second");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second).await;
    let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;

    let third = fixture.commit_and_push("third");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third).await;

    for _ in 0..100 {
        if let Some(head) = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
        {
            let chain = materializer
                .generation_chain(&fixture.repo, head.generation)
                .await
                .unwrap();
            if chain.len() == 1 && head.generation != third_manifest.generation {
                let old_generations_deleted = read_generation_manifest(
                    &*state.store,
                    &fixture.repo,
                    first_manifest.generation,
                )
                .await
                .unwrap()
                .is_none()
                    && read_generation_manifest(
                        &*state.store,
                        &fixture.repo,
                        second_manifest.generation,
                    )
                    .await
                    .unwrap()
                    .is_none()
                    && read_generation_manifest(
                        &*state.store,
                        &fixture.repo,
                        third_manifest.generation,
                    )
                    .await
                    .unwrap()
                    .is_none();
                if old_generations_deleted {
                    return;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("inline compaction did not collapse the verified generation chain");
}

#[tokio::test]
async fn compaction_preserves_parents_needed_by_pending_generation_verification() {
    let fixture = GitFixture::new();
    let config = AppConfig {
        compaction: git_cache_core::CompactionConfig {
            chain_depth_threshold: 1,
            inline: false,
        },
        ..fixture.state_config()
    };
    let state = Arc::new(AppState::try_new(config).unwrap());
    let materializer = Materializer::new(Arc::clone(&state));

    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let second_commit = fixture.commit_and_push("second");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
    let _ = wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;

    let third_commit = fixture.commit_and_push("third");
    materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
            upstream_authorization: Default::default(),
        })
        .await
        .unwrap();
    let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third_commit).await;
    let parent_head =
        wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;

    let child_commit = fixture.commit_and_push("fourth");
    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    run_git(
        &repo_dir,
        [
            "fetch",
            "--no-tags",
            fixture.upstream_path().to_str().unwrap(),
            "+refs/heads/main:refs/cache/upstream/heads/main",
        ],
    );

    let child_generation = GenerationId::new();
    let reservation = state.disk.reserve(1024 * 1024 * 64).await.unwrap();
    let temp_path = reservation.temp_path().unwrap();
    fs::create_dir_all(&temp_path).await.unwrap();
    let bundle_path = temp_path.join("pending-child.bundle");
    state
        .git
        .bundle_create_incremental(&repo_dir, &bundle_path, &parent_head.tip_commits)
        .await
        .unwrap();

    let now = Utc::now();
    let child_manifest = GenerationManifest {
        repo: fixture.repo.clone(),
        generation: child_generation,
        bundle_key: bundle_key(&fixture.repo, child_generation),
        parent_generation: Some(parent_head.generation),
        created_at: now,
        commits: vec![child_commit.clone()],
    };
    let mut child_tip_commits = parent_head.tip_commits.clone();
    push_unique_commit(&mut child_tip_commits, child_commit.clone());
    let child_head = RepoGenerationHead {
        repo: fixture.repo.clone(),
        generation: child_generation,
        tip_commits: child_tip_commits,
        updated_at: now,
    };
    let child_manifests = PublishManifests {
        commits: vec![CommitManifest {
            repo: fixture.repo.clone(),
            commit: child_commit,
            generation: child_generation,
            complete: true,
            verified_at: now,
        }],
        refs: Vec::new(),
        sessions: Vec::new(),
    };
    GenerationPublish::with_manifests(child_manifest, child_manifests)
        .publish_pending_bundle_file(&*state.store, &bundle_path, child_head, None)
        .await
        .unwrap();
    reservation.release().await.unwrap();

    let report = materializer
        .compact_generation_chain(&fixture.repo)
        .await
        .unwrap()
        .unwrap();
    assert!(report.old_generations.contains(&parent_head.generation));

    materializer
        .verify_generation_with_semaphore(fixture.repo.clone(), child_generation, false)
        .await
        .expect("pending generation should verify after parent chain compaction");
}

// ── upstream_url tests ───────────────────────────────────────────
