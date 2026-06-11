mod tests {
    use super::super::*;
    use git_cache_objectstore::write_repo_generation_head;

    #[tokio::test]
    async fn publish_generation_links_delta_to_previous_generation() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
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
        let first_generation =
            generation_manifest_for(&state, &fixture.repo, first_manifest.generation).await;
        assert_eq!(first_generation.packs.len(), 1);

        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let second_generation =
            generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(second_generation.packs.len(), 2);
        assert!(second_generation
            .packs
            .iter()
            .any(|pack| pack.key == first_generation.packs[0].key));

        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(head.generation, second_manifest.generation);
        assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
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
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
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
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert_eq!(response.commit, ancestor_commit);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
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
    async fn exact_ancestor_hydrates_generation_before_broad_upstream_fetch() {
        let fixture = GitFixture::new();
        let ancestor_commit = fixture.head_commit();
        let tip_commit = fixture.commit_and_push("second");
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
        wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;
        let head_before =
            Some(wait_for_generation_head(&state, &fixture.repo, tip_manifest.generation).await);
        let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

        stdfs::remove_dir_all(materializer.repo_dir(&fixture.repo)).unwrap();
        let replacement = fixture.replace_history_and_push("replacement");
        assert_ne!(replacement, ancestor_commit);
        assert_ne!(replacement, tip_commit);

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(ancestor_commit.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert_eq!(response.commit, ancestor_commit);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
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
    async fn exact_ancestor_missing_generation_packs_falls_back_to_upstream_fetch() {
        let fixture = GitFixture::new();
        let ancestor_commit = fixture.head_commit();
        let tip_commit = fixture.commit_and_push("second");
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let tip_manifest = wait_for_commit_manifest(&state, &fixture.repo, &tip_commit).await;
        wait_for_verified_generation(&state, &fixture.repo, tip_manifest.generation).await;

        let tip_generation =
            generation_manifest_for(&state, &fixture.repo, tip_manifest.generation).await;
        for pack in &tip_generation.packs {
            state.store.delete(&pack.key).await.unwrap();
        }
        stdfs::remove_dir_all(materializer.repo_dir(&fixture.repo)).unwrap();

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(ancestor_commit.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.commit, ancestor_commit);
        assert_eq!(response.source, MaterializeSource::UpstreamVerified);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .unwrap();
        assert_ne!(ancestor_manifest.generation, tip_manifest.generation);
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
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.source, MaterializeSource::CacheVerified);
        assert_eq!(response.commit, ancestor_commit);

        let ancestor_manifest =
            read_commit_manifest(&*state.store, &fixture.repo, &ancestor_commit)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(ancestor_manifest.generation, tip_manifest.generation);
        let generation_keys_after = generation_object_keys(&state, &fixture.repo).await;
        assert_eq!(generation_keys_after, generation_keys_before);
    }

    #[tokio::test]
    async fn exact_descendants_after_cold_ancestor_fetch_reuse_full_generation() {
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
    async fn exact_commit_ahead_of_known_generation_publishes_incremental_pack() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
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
        let second_commit = fixture.commit_and_push("second");

        let response = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(second_commit.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.commit, second_commit);
        assert_eq!(response.source, MaterializeSource::UpstreamVerified);

        let second_manifest = wait_for_commit_manifest(&state, &fixture.repo, &second_commit).await;
        let first_generation =
            generation_manifest_for(&state, &fixture.repo, first_manifest.generation).await;
        let second_generation =
            generation_manifest_for(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(second_generation.packs.len(), 2);
        assert!(second_generation
            .packs
            .iter()
            .any(|pack| pack.key == first_generation.packs[0].key));
        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
        assert_eq!(head.tip_commits, vec![first.commit, second_commit]);
    }

    #[tokio::test]
    async fn delta_publish_falls_back_to_full_pack_when_previous_tip_is_missing_locally() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let first = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
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
        assert_eq!(second_generation.packs.len(), 1);
        let head =
            wait_for_generation_head(&state, &fixture.repo, second_manifest.generation).await;
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
                retention_secs: 0,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
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
        let second_commit = fixture.commit_and_push("second");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
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
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        let head_before = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        let old_pack_keys: Vec<String> =
            generation_manifest_for(&state, &fixture.repo, head_before.generation)
                .await
                .packs
                .iter()
                .map(|pack| pack.key.clone())
                .collect();

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.old_pack_count, 3);

        let head = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head.generation, report.new_generation);
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        assert_eq!(compacted.packs.len(), 1);

        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        for old_key in &old_pack_keys {
            if compacted.packs.iter().all(|pack| pack.key != *old_key) {
                assert!(!state.store.exists(old_key).await.unwrap());
            }
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
    async fn retention_sweep_keeps_recently_superseded_generations() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
                retention_secs: 24 * 60 * 60,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
        let materializer = Materializer::new(Arc::clone(&state));

        for message in ["second", "third"] {
            materializer
                .materialize(MaterializeRequest {
                    repo: fixture.repo.clone(),
                    selector: Selector::Branch(BranchName::parse("main").unwrap()),
                    upstream_authorization: Default::default(),
                })
                .await
                .unwrap();
            fixture.commit_and_push(message);
        }
        let last = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let last_manifest = wait_for_commit_manifest(&state, &fixture.repo, &last.commit).await;
        let _ = wait_for_generation_head(&state, &fixture.repo, last_manifest.generation).await;

        let head_before = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        let old_pack_keys: Vec<String> =
            generation_manifest_for(&state, &fixture.repo, head_before.generation)
                .await
                .packs
                .iter()
                .map(|pack| pack.key.clone())
                .collect();

        let report = materializer
            .compact_generation_chain(&fixture.repo)
            .await
            .unwrap()
            .unwrap();

        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_some(),
                "superseded generation must be retained within the retention window"
            );
        }
        for old_key in &old_pack_keys {
            assert!(
                state.store.exists(old_key).await.unwrap(),
                "superseded packs must be retained within the retention window"
            );
        }

        let zero_retention = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
                retention_secs: 0,
            },
            ..fixture.state_config()
        };
        let sweeper_state = Arc::new(AppState::try_new(zero_retention).unwrap());
        let sweeper = Materializer::new(Arc::clone(&sweeper_state));
        let sweep = sweeper
            .sweep_superseded_generations(&fixture.repo)
            .await
            .unwrap();
        assert!(!sweep.swept_generations.is_empty());

        for old_generation in &report.old_generations {
            assert!(
                read_generation_manifest(&*state.store, &fixture.repo, *old_generation)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        let compacted = generation_manifest_for(&state, &fixture.repo, report.new_generation).await;
        for old_key in &old_pack_keys {
            if compacted.packs.iter().all(|pack| pack.key != *old_key) {
                assert!(!state.store.exists(old_key).await.unwrap());
            }
        }
        let head_after = read_repo_generation_head(&*state.store, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(head_after.generation, report.new_generation);
        assert!(
            read_generation_manifest(&*state.store, &fixture.repo, report.new_generation)
                .await
                .unwrap()
                .is_some(),
            "head generation must always survive the sweep"
        );
    }

    #[tokio::test]
    async fn cold_node_serves_commit_after_sweep_removes_superseded_generation() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 64,
                inline: false,
                retention_secs: 0,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
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

        let sweep = materializer
            .sweep_superseded_generations(&fixture.repo)
            .await
            .unwrap();
        assert!(sweep.swept_generations.contains(&first_manifest.generation));
        assert!(
            read_generation_manifest(&*state.store, &fixture.repo, first_manifest.generation)
                .await
                .unwrap()
                .is_none()
        );

        // A node with a cold disk cache (same object store, fresh cache root)
        // must still be able to serve the first commit: its manifest may not
        // be left pointing at the swept generation.
        let cold_config = AppConfig {
            cache_root: fixture.tmp.path().join("cache-cold"),
            ..fixture.state_config()
        };
        let cold_state = Arc::new(AppState::try_new(cold_config).unwrap());
        let cold = Materializer::new(Arc::clone(&cold_state));
        let response = cold
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Commit(first.commit.clone()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.commit, first.commit);
        let cold_repo_dir = cold.repo_dir(&fixture.repo);
        assert!(cold.commit_exists(&cold_repo_dir, &first.commit).await);
    }

    #[tokio::test]
    async fn retention_sweep_collects_orphaned_packs_by_age() {
        use git_cache_objectstore::pack_prefix;

        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
                retention_secs: 0,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
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

        let orphan_key = format!("{}pack-{}.pack", pack_prefix(&fixture.repo), "f".repeat(64));
        state
            .store
            .put(&orphan_key, bytes::Bytes::from_static(b"leaked"))
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let sweep = materializer
            .sweep_superseded_generations(&fixture.repo)
            .await
            .unwrap();
        assert!(sweep.deleted_packs >= 1);
        assert!(!state.store.exists(&orphan_key).await.unwrap());

        let head_manifest =
            generation_manifest_for(&state, &fixture.repo, first_manifest.generation).await;
        for pack in &head_manifest.packs {
            assert!(state.store.exists(&pack.key).await.unwrap());
        }
    }

    #[tokio::test]
    async fn inline_compaction_runs_after_verified_head_update() {
        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: true,
                retention_secs: 0,
            },
            ..fixture.state_config()
        };
        let state = Arc::new(AppState::try_new(config).unwrap());
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

        for _ in 0..100 {
            if let Some(head) = read_repo_generation_head(&*state.store, &fixture.repo)
                .await
                .unwrap()
            {
                let head_manifest =
                    read_generation_manifest(&*state.store, &fixture.repo, head.generation)
                        .await
                        .unwrap();
                let single_pack = head_manifest.is_some_and(|manifest| manifest.packs.len() == 1);
                if single_pack && head.generation != third_manifest.generation {
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

    // ── upstream_url tests ───────────────────────────────────────────

    use git_cache_objectstore::{ObjectStore, ObjectVersion};

    /// Object store wrapper that pauses the first write to the generation
    /// head pointer while armed, letting the test interleave a concurrent
    /// publish (as another node would) between compaction's reads and its
    /// head write + pack cleanup.
    struct HeadWriteGate {
        inner: Arc<dyn ObjectStore>,
        head_key: String,
        armed: std::sync::atomic::AtomicBool,
        hit: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    impl HeadWriteGate {
        async fn pause_if_armed(&self, key: &str) {
            if key == self.head_key && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.hit.notify_one();
                self.resume.notified().await;
            }
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for HeadWriteGate {
        async fn get(&self, key: &str) -> CoreResult<Option<bytes::Bytes>> {
            self.inner.get(key).await
        }

        async fn put(&self, key: &str, value: bytes::Bytes) -> CoreResult<()> {
            self.pause_if_armed(key).await;
            self.inner.put(key, value).await
        }

        async fn put_if_absent(&self, key: &str, value: bytes::Bytes) -> CoreResult<bool> {
            self.inner.put_if_absent(key, value).await
        }

        async fn get_versioned(
            &self,
            key: &str,
        ) -> CoreResult<Option<(bytes::Bytes, ObjectVersion)>> {
            self.inner.get_versioned(key).await
        }

        async fn put_if_version_matches(
            &self,
            key: &str,
            value: bytes::Bytes,
            version: &ObjectVersion,
        ) -> CoreResult<bool> {
            self.pause_if_armed(key).await;
            self.inner.put_if_version_matches(key, value, version).await
        }

        async fn exists(&self, key: &str) -> CoreResult<bool> {
            self.inner.exists(key).await
        }

        async fn delete(&self, key: &str) -> CoreResult<()> {
            self.inner.delete(key).await
        }

        async fn list_prefix(
            &self,
            prefix: &str,
            max_keys: Option<usize>,
        ) -> CoreResult<Vec<String>> {
            self.inner.list_prefix(prefix, max_keys).await
        }

        async fn head(&self, key: &str) -> CoreResult<Option<git_cache_objectstore::ObjectMeta>> {
            self.inner.head(key).await
        }
    }

    /// Regression test for the compaction/publish TOCTOU: a generation
    /// published concurrently (e.g. by another node) between compaction's
    /// reads and its head write must survive — the CAS head write makes
    /// compaction abort its cleanup instead of clobbering the publish and
    /// deleting the packs it references.
    #[tokio::test]
    async fn compaction_preserves_generation_published_concurrently() {
        use git_cache_objectstore::{
            repo_generation_head_key, write_generation_manifest, write_repo_generation_head,
        };

        let fixture = GitFixture::new();
        let config = AppConfig {
            compaction: git_cache_core::CompactionConfig {
                chain_depth_threshold: 2,
                inline: false,
                retention_secs: 24 * 60 * 60,
            },
            ..fixture.state_config()
        };
        let inner: Arc<dyn ObjectStore> = Arc::new(git_cache_objectstore::LocalObjectStore::new(
            fixture.tmp.path().join("objects"),
        ));
        let gate = Arc::new(HeadWriteGate {
            inner: Arc::clone(&inner),
            head_key: repo_generation_head_key(&fixture.repo),
            armed: std::sync::atomic::AtomicBool::new(false),
            hit: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let git = git_cache_git::Git::with_concurrency_limit(
            config.git_binary.clone(),
            std::time::Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes);
        let disk = git_cache_disk::DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        let state = Arc::new(AppState {
            config,
            store: Arc::clone(&gate) as Arc<dyn ObjectStore>,
            git,
            disk: git_cache_disk::AsyncDiskManager::new(disk),
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
        let mut last_commit = first.commit.clone();
        for contents in ["second", "third"] {
            last_commit = fixture.commit_and_push(contents);
            materializer
                .materialize(MaterializeRequest {
                    repo: fixture.repo.clone(),
                    selector: Selector::Branch(BranchName::parse("main").unwrap()),
                    upstream_authorization: Default::default(),
                })
                .await
                .unwrap();
            let manifest = wait_for_commit_manifest(&state, &fixture.repo, &last_commit).await;
            let _ = wait_for_generation_head(&state, &fixture.repo, manifest.generation).await;
        }
        let last_manifest = wait_for_commit_manifest(&state, &fixture.repo, &last_commit).await;
        let head_before =
            wait_for_generation_head(&state, &fixture.repo, last_manifest.generation).await;
        let head_manifest =
            generation_manifest_for(&state, &fixture.repo, head_before.generation).await;
        assert!(head_manifest.packs.len() > 2, "need a compactable chain");

        // The "concurrent publish": a generation created by another node in
        // the window between compaction's reads and its head write. It
        // references the packs that existed when it was published. It is
        // written through the inner store directly, bypassing this process's
        // repo lock, exactly as a second node would.
        let published_generation = GenerationId::new();
        let published_manifest = GenerationManifest {
            repo: fixture.repo.clone(),
            generation: published_generation,
            created_at: Utc::now(),
            verified_at: Some(Utc::now()),
            packs: head_manifest.packs.clone(),
            refs: head_manifest.refs.clone(),
            head_ref: head_manifest.head_ref.clone(),
            commits: head_manifest.commits.clone(),
        };
        let published_head = RepoGenerationHead {
            repo: fixture.repo.clone(),
            generation: published_generation,
            tip_commits: head_before.tip_commits.clone(),
            updated_at: Utc::now(),
        };

        gate.armed.store(true, std::sync::atomic::Ordering::SeqCst);
        let inject = async {
            gate.hit.notified().await;
            write_generation_manifest(&*inner, &published_manifest)
                .await
                .unwrap();
            write_repo_generation_head(&*inner, &published_head)
                .await
                .unwrap();
            gate.resume.notify_one();
        };
        let (report, ()) =
            tokio::join!(materializer.compact_generation_chain(&fixture.repo), inject);
        report.unwrap();

        let head_after = read_repo_generation_head(&*inner, &fixture.repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            head_after.generation, published_generation,
            "compaction must not clobber a generation head published concurrently"
        );
        let surviving = read_generation_manifest(&*inner, &fixture.repo, published_generation)
            .await
            .unwrap()
            .expect("concurrently published generation manifest must survive");
        for pack in &surviving.packs {
            assert!(
                inner.exists(&pack.key).await.unwrap(),
                "pack `{}` referenced by the concurrently published generation was deleted",
                pack.key
            );
        }
    }
}
