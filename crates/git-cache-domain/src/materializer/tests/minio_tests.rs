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
        assert!(state
            .store
            .head(&bundle_key(&fixture.repo, manifest.generation))
            .await
            .unwrap()
            .is_some());

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
            generation_verification_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
            serving_maintenance_inflight: Arc::new(std::sync::Mutex::new(
                std::collections::HashSet::new(),
            )),
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
        assert_eq!(report.old_chain_depth, 3);
        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(state
                .store
                .head(&bundle_key(&fixture.repo, *old_generation))
                .await
                .unwrap()
                .is_none());
        }
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        assert_eq!(compacted.parent_generation, None);

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
}
