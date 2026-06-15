mod tests {
    use super::super::*;

    #[cfg(feature = "s3-tests")]
    #[tokio::test]
    async fn minio_materializer_rehydrates_commit_from_minio_after_hot_cache_deletion() {
        let Some(minio) = MinioFixture::new().await else {
            eprintln!("skipping minio_materializer_rehydrates_commit_from_minio_after_hot_cache_deletion: set GIT_CACHE_S3_INTEGRATION=1");
            return;
        };
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state_with_store(Arc::clone(&minio.store)));
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &first.commit).await;
        let generation = generation_manifest_for(&state, &fixture.repo, manifest.generation).await;
        assert!(!generation.packs.is_empty());
        for pack in &generation.packs {
            assert!(state.store.head(&pack.key).await.unwrap().is_some());
        }

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(first.commit.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert!(materializer.commit_exists(&repo_dir, &first.commit).await);
    }

    #[cfg(feature = "s3-tests")]
    #[tokio::test]
    async fn minio_materializer_compacts_generations_and_rehydrates_commits() {
        let Some(minio) = MinioFixture::new().await else {
            eprintln!("skipping minio_materializer_compacts_generations_and_rehydrates_commits: set GIT_CACHE_S3_INTEGRATION=1");
            return;
        };
        let fixture = GitFixture::new();
        let mut config = fixture.state_config();
        config.compaction = git_cache_core::CompactionConfig {
            chain_depth_threshold: 2,
            inline: false,
            retention_secs: 0,
        };
        let git = Git::with_concurrency_limit(
            config.git_binary.clone(),
            std::time::Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        let state = Arc::new(AppState {
            config,
            store: Arc::clone(&minio.store),
            git,
            disk: AsyncDiskManager::new(disk),
            serving_maintenance_inflight: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
            repo_mutation_locks: Arc::new(
                tokio::sync::Mutex::new(std::collections::HashMap::new()),
            ),
        });
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
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
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let third_manifest = wait_for_commit_manifest(&state, &fixture.repo, &third).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, third_manifest.generation).await;

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.old_pack_count, 3);
        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        assert_eq!(compacted.packs.len(), 1);

        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();
        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(third.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);

        for commit in [first.commit, second, third] {
            materializer
                .state
                .git
                .rev_parse(&repo_dir, &format!("{}^{{commit}}", commit.as_str()))
                .await
                .unwrap();
        }
    }

    /// Two independent nodes (separate disk caches, shared bucket) racing
    /// publishes against compaction+sweep must leave every published commit
    /// servable by a third cold node.
    #[cfg(feature = "s3-tests")]
    #[tokio::test]
    async fn minio_two_node_publish_compact_sweep_race_keeps_commits_servable() {
        let Some(minio) = MinioFixture::new().await else {
            eprintln!("skipping minio_two_node_publish_compact_sweep_race_keeps_commits_servable: set GIT_CACHE_S3_INTEGRATION=1");
            return;
        };
        let fixture = GitFixture::new();
        let make_state = |cache_dir: &str, retention_secs: u64| {
            let mut config = fixture.state_config();
            config.cache_root = fixture.tmp.path().join(cache_dir);
            config.compaction = git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
                retention_secs,
            };
            let git = Git::with_concurrency_limit(
                config.git_binary.clone(),
                std::time::Duration::from_secs(config.git_timeout_seconds),
                config.max_concurrent_git_processes,
            )
            .with_output_limit(config.max_git_output_bytes);
            let disk = DiskManager::new(
                &config.cache_root,
                config.disk.quota_bytes,
                config.disk.min_free_bytes,
            );
            Arc::new(AppState {
                config,
                store: Arc::clone(&minio.store),
                git,
                disk: AsyncDiskManager::new(disk),
                serving_maintenance_inflight: Arc::new(std::sync::Mutex::new(
                    std::collections::HashSet::new(),
                )),
                repo_mutation_locks: Arc::new(tokio::sync::Mutex::new(
                    std::collections::HashMap::new(),
                )),
            })
        };
        let node_a = make_state("cache-a", 3600);
        let node_b = make_state("cache-b", 3600);

        let mut commits = vec![fixture.head_commit()];
        for round in 0..4 {
            let publisher = Materializer::new(Arc::clone(&node_a));
            let publish_repo = fixture.repo.clone();
            let publish = tokio::spawn(async move {
                publisher
                    .materialize(MaterializeRequest {
                        repo: publish_repo,
                        selector: Selector::Branch(BranchName::parse("main").unwrap()),
                        upstream_authorization: Default::default(),
                    })
                    .await
            });
            let compactor = Materializer::new(Arc::clone(&node_b));
            let compact_repo = fixture.repo.clone();
            let compact =
                tokio::spawn(
                    async move { compactor.compact_generation_chain(&compact_repo).await },
                );
            let (publish, compact) = tokio::join!(publish, compact);
            publish.unwrap().unwrap();
            compact.unwrap().unwrap();
            commits.push(fixture.commit_and_push(&format!("round-{round}")));
        }
        Materializer::new(Arc::clone(&node_a))
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        let sweeper = Materializer::new(make_state("cache-sweeper", 0));
        let sweep = sweeper
            .sweep_superseded_generations(&fixture.repo)
            .await
            .unwrap();
        assert!(!sweep.swept_generations.is_empty());

        let cold = Materializer::new(make_state("cache-cold", 3600));
        for commit in &commits {
            let response = cold
                .materialize(MaterializeRequest {
                    repo: fixture.repo.clone(),
                    selector: Selector::Commit(commit.clone()),
                    upstream_authorization: Default::default(),
                })
                .await
                .unwrap();
            assert_eq!(response.commit, *commit);
        }
    }
}
