mod tests {
    use super::super::*;

    #[test]
    fn synthesize_ref_advertisement_contains_head_and_refs() {
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "a".repeat(40));
                m.insert("develop".to_string(), "b".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);
        let text = String::from_utf8_lossy(&output);

        assert!(text.contains("HEAD"));
        assert!(text.contains("refs/heads/main"));
        assert!(text.contains("refs/heads/develop"));
        assert!(text.ends_with("0000"));
    }

    #[test]
    fn synthesize_ref_advertisement_valid_pkt_line_format() {
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "c".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);

        // First 4 bytes are hex length
        assert!(output.len() >= 4);
        let first_len_str = std::str::from_utf8(&output[..4]).unwrap();
        let first_len: usize = usize::from_str_radix(first_len_str, 16).unwrap();
        assert!(first_len > 4);

        // Ends with flush packet
        assert!(output.ends_with(b"0000"));
    }

    #[test]
    fn synthesize_ref_advertisement_contains_capability_line() {
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: {
                let mut m = HashMap::new();
                m.insert("main".to_string(), "d".repeat(40));
                m
            },
        };
        let output = synthesize_ref_advertisement(&comparison);
        let text = String::from_utf8_lossy(&output);

        assert!(text.contains("multi_ack"));
        assert!(text.contains("agent=git-cache/1.0"));
    }

    fn make_upload_pack_pkt_line(data: &str) -> Vec<u8> {
        format!("{:04x}{data}", data.len() + 4).into_bytes()
    }

    #[test]
    fn upload_pack_blobless_filter_parser_detects_exact_filter_line() {
        let mut body = make_upload_pack_pkt_line("want aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n");
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        body.extend(make_upload_pack_pkt_line("done\n"));

        assert!(super::super::direct_git::upload_pack_requests_blobless_filter(&body));
    }

    #[test]
    fn upload_pack_blobless_filter_parser_ignores_other_filters() {
        let mut body = make_upload_pack_pkt_line("filter tree:0\n");
        body.extend(make_upload_pack_pkt_line("filter blob:limit=10\n"));

        assert!(!super::super::direct_git::upload_pack_requests_blobless_filter(&body));
    }

    #[test]
    fn upload_pack_intent_parser_preserves_depth_and_blobless_filter() {
        let sha = "a".repeat(40);
        let mut body = make_upload_pack_pkt_line(&format!("want {sha} multi_ack thin-pack\n"));
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("deepen 1\n"));
        body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        body.extend(make_upload_pack_pkt_line("done\n"));

        let intent = super::super::direct_git::parse_upload_pack_intent(&body).unwrap();

        assert_eq!(intent.wants, vec![CommitSha::parse(&sha).unwrap()]);
        assert_eq!(
            intent.filter,
            Some(super::super::direct_git::UploadPackFilter::BlobNone)
        );
        assert_eq!(intent.depth, Some(1));
    }

    #[test]
    fn upload_pack_intent_parser_ignores_unsupported_filters() {
        let sha = "b".repeat(40);
        let mut body = make_upload_pack_pkt_line(&format!("want {sha}\n"));
        body.extend(make_upload_pack_pkt_line("filter tree:0\n"));

        let intent = super::super::direct_git::parse_upload_pack_intent(&body).unwrap();

        assert_eq!(intent.wants, vec![CommitSha::parse(&sha).unwrap()]);
        assert_eq!(intent.filter, None);
    }

    #[test]
    fn upload_pack_intent_parser_stops_on_invalid_packet_length() {
        let intent = super::super::direct_git::parse_upload_pack_intent(b"zzzzbogus data here")
            .expect("malformed pkt-line prefixes should stop parsing");

        assert!(intent.wants.is_empty());
        assert_eq!(intent.filter, None);
    }

    #[test]
    fn upload_pack_intent_parser_stops_on_truncated_packet() {
        let intent = super::super::direct_git::parse_upload_pack_intent(b"0032want short\n")
            .expect("truncated pkt-lines should stop parsing");

        assert!(intent.wants.is_empty());
        assert_eq!(intent.filter, None);
    }

    #[tokio::test]
    async fn direct_want_allows_locally_ready_commit_after_repo_access() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        stdfs::write(fixture.work_path().join("private.txt"), "private\n").unwrap();
        run_git(&fixture.work_path(), ["add", "private.txt"]);
        run_git(
            &fixture.work_path(),
            ["commit", "-m", "private local commit"],
        );
        let private_commit =
            CommitSha::parse(git_stdout(&fixture.work_path(), ["rev-parse", "HEAD"])).unwrap();
        run_git(
            &repo_dir,
            [
                "fetch",
                fixture.work_path().to_str().unwrap(),
                "HEAD:refs/cache/private",
            ],
        );

        assert!(materializer.commit_exists(&repo_dir, &private_commit).await);

        materializer
            .ensure_wants_available(&fixture.repo, &[private_commit.to_string()])
            .await
            .expect("repo-authorized direct Git wants use object presence as availability");
    }

    #[tokio::test]
    async fn anonymous_direct_want_allows_cached_public_ancestor_when_current_tip_is_local() {
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
        let descendant = fixture.commit_and_push("public descendant");
        materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &descendant)
                .await,
            "current advertised tip must be local before direct Git can prove ancestor wants"
        );

        materializer
            .ensure_wants_available(&fixture.repo, &[first.commit.to_string()])
            .await
            .expect("anonymous wants should allow commits reachable from public upstream refs");
    }

    #[tokio::test]
    async fn anonymous_direct_want_for_cached_ancestor_without_local_current_tip_stays_local_only()
    {
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
        let descendant = fixture.commit_and_push("public descendant");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        materializer
            .ensure_wants_available(&fixture.repo, &[first.commit.to_string()])
            .await
            .expect("repo-authorized direct Git wants should not need current-tip proof");
        assert!(
            !materializer.commit_exists(&repo_dir, &descendant).await,
            "direct Git POST must not import the current upstream tip for reachability proof"
        );
    }

    #[tokio::test]
    async fn anonymous_direct_want_uses_cached_local_objects_when_upstream_is_offline() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        let public_main = state
            .git
            .rev_parse(&repo_dir, "refs/heads/main")
            .await
            .unwrap();
        assert_eq!(public_main, cached.commit.to_string());

        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        materializer
            .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
            .await
            .expect("direct Git POST should not require upstream proof after repo access");
    }

    #[tokio::test]
    async fn anonymous_direct_want_for_advertised_uncached_commit_reads_through() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let commit = fixture.head_commit();
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        assert!(
            !materializer.object_exists(&repo_dir, &commit).await,
            "fixture should start with an empty local cache"
        );

        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
        };

        let result = materializer
            .ensure_wants_available_from_comparison(
                &fixture.repo,
                &[commit.to_string()],
                &comparison,
                false,
            )
            .await;

        assert!(
            result.is_ok(),
            "direct Git POST should read through an authorized cache miss: {result:?}"
        );
        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &commit)
                .await
        );
        assert_eq!(
            state
                .git
                .rev_parse(&repo_dir, &format!("refs/cache/commits/{commit}"))
                .await
                .unwrap(),
            commit.to_string()
        );
        assert!(
            materializer
                .get_commit_manifest(&fixture.repo, &commit)
                .await
                .unwrap()
                .is_none(),
            "direct Git read-through should not publish generations synchronously"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            materializer
                .get_commit_manifest(&fixture.repo, &commit)
                .await
                .unwrap()
                .is_none(),
            "direct Git read-through should trigger fsck, not generation publication"
        );
    }

    #[tokio::test]
    async fn upload_pack_cache_prepare_is_false_for_uncached_want() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let commit = fixture.head_commit();
        let body = make_upload_pack_pkt_line(&format!("want {commit} multi_ack thin-pack\n"));

        assert!(
            !materializer
                .prepare_upload_pack_from_cache(&fixture.repo, &Bytes::from(body))
                .await
                .unwrap(),
            "cold proxy mode should not treat missing local objects as cheaply serveable"
        );
    }

    #[tokio::test]
    async fn upload_pack_cache_prepare_is_false_for_manifest_only_cache() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, manifest.generation).await;
        let repo_dir = materializer.repo_dir(&fixture.repo);
        stdfs::remove_dir_all(&repo_dir).unwrap();

        let body =
            make_upload_pack_pkt_line(&format!("want {} multi_ack thin-pack\n", cached.commit));

        assert!(
            !materializer
                .prepare_upload_pack_from_cache(&fixture.repo, &Bytes::from(body))
                .await
                .unwrap(),
            "proxy-on-miss should use upstream proxying when only object-store generation manifests exist"
        );
        assert!(
            !materializer
                .commit_ready_for_serving(&repo_dir, &cached.commit)
                .await,
            "cache prepare should not hydrate EBS from the verified generation"
        );
    }

    #[tokio::test]
    async fn upload_pack_cache_prepare_is_true_for_hot_commit() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let body =
            make_upload_pack_pkt_line(&format!("want {} multi_ack thin-pack\n", cached.commit));

        assert!(
            materializer
                .prepare_upload_pack_from_cache(&fixture.repo, &Bytes::from(body))
                .await
                .unwrap(),
            "hot direct clone should continue down the local upload-pack path"
        );
    }

    #[tokio::test]
    async fn warm_upload_pack_fetches_upstream_when_manifest_bundle_is_unavailable() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let parent = fixture.head_commit();
        let commit = fixture.commit_and_push("second");

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(cached.commit, commit);
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, manifest.generation).await;

        delete_generation_packs(&state, &fixture.repo, manifest.generation).await;
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        stdfs::remove_dir_all(&repo_dir).unwrap();

        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), cached.commit.to_string())]),
        };
        let mut body =
            make_upload_pack_pkt_line(&format!("want {} multi_ack thin-pack\n", cached.commit));
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("deepen 1\n"));
        body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        body.extend(make_upload_pack_pkt_line("done\n"));

        let result = materializer
            .warm_upload_pack(&fixture.repo, &Bytes::from(body), Some(&comparison))
            .await;

        assert!(
            result.is_ok(),
            "background warm should fetch from upstream instead of depending on generation hydrate: {result:?}"
        );
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &cached.commit)
                .await,
            "background warm should make the wanted commit ready"
        );
        assert!(
            !materializer.commit_exists(&repo_dir, &parent).await,
            "background warm should preserve the client's shallow depth"
        );
    }

    #[tokio::test]
    async fn authenticated_direct_want_for_advertised_uncached_commit_reads_through() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();
        let authed_materializer = materializer.using_upstream_auth(&auth);
        let commit = fixture.head_commit();
        let repo_dir = authed_materializer
            .ensure_repo_dir(&fixture.repo)
            .await
            .unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
        };

        let result = authed_materializer
            .ensure_wants_available_from_comparison(
                &fixture.repo,
                &[commit.to_string()],
                &comparison,
                false,
            )
            .await;

        assert!(
            result.is_ok(),
            "authenticated direct Git POST should read through local cache misses: {result:?}"
        );
        assert!(
            authed_materializer
                .commit_ready_for_serving(&repo_dir, &commit)
                .await
        );
    }

    #[tokio::test]
    async fn direct_want_for_advertised_branch_preserves_depth_and_fetches_ref() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let parent = fixture.head_commit();
        let commit = fixture.commit_and_push("second");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
        };
        let mut body = make_upload_pack_pkt_line(&format!("want {commit} multi_ack thin-pack\n"));
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("deepen 1\n"));
        body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        body.extend(make_upload_pack_pkt_line("done\n"));
        let intent = super::super::direct_git::parse_upload_pack_intent(&body).unwrap();

        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &intent,
                &comparison,
            )
            .await
            .expect("advertised branch wants should read through with client depth/filter");

        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &commit)
                .await
        );
        assert_eq!(
            state
                .git
                .rev_parse(&repo_dir, "refs/cache/upstream/heads/main")
                .await
                .unwrap(),
            commit.to_string(),
            "advertised branch wants should fetch the ref instead of only the raw SHA"
        );
        assert!(
            !materializer.commit_exists(&repo_dir, &parent).await,
            "deepen 1 should avoid importing the parent commit on a cold read-through"
        );
    }

    #[tokio::test]
    async fn blobless_hydrated_repo_refetches_full_objects_for_unfiltered_wants() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let commit = fixture.head_commit();
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
        };

        let mut blobless = make_upload_pack_pkt_line(&format!("want {commit} multi_ack\n"));
        blobless.extend_from_slice(b"0000");
        blobless.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        blobless.extend(make_upload_pack_pkt_line("done\n"));
        let blobless_intent =
            super::super::direct_git::parse_upload_pack_intent(&blobless).unwrap();
        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &blobless_intent,
                &comparison,
            )
            .await
            .expect("blobless read-through should hydrate the repo");

        let marker = repo_dir.join("git-cache-partial-hydration");
        assert!(
            marker.exists(),
            "blobless hydration should mark the repo as partially hydrated"
        );
        assert!(
            !materializer
                .prepare_upload_pack_from_cache(
                    &fixture.repo,
                    &Bytes::from(make_full_body(&commit))
                )
                .await
                .unwrap(),
            "partially hydrated repos must decline full-object cache prepare"
        );

        let full_intent =
            super::super::direct_git::parse_upload_pack_intent(&make_full_body(&commit)).unwrap();
        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &full_intent,
                &comparison,
            )
            .await
            .expect("full read-through against a partially hydrated repo should refetch");

        assert!(
            !marker.exists(),
            "full refetch should clear the partial hydration marker"
        );
        assert!(
            materializer
                .prepare_upload_pack_from_cache(
                    &fixture.repo,
                    &Bytes::from(make_full_body(&commit))
                )
                .await
                .unwrap(),
            "fully refetched repos should serve full-object shapes from cache"
        );
    }

    #[tokio::test]
    async fn shallow_hydrated_repo_unshallows_for_full_history_wants() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let parent = fixture.head_commit();
        let commit = fixture.commit_and_push("second");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
        };

        let mut shallow_body = make_upload_pack_pkt_line(&format!("want {commit} multi_ack\n"));
        shallow_body.extend_from_slice(b"0000");
        shallow_body.extend(make_upload_pack_pkt_line("deepen 1\n"));
        shallow_body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        shallow_body.extend(make_upload_pack_pkt_line("done\n"));
        let shallow_intent =
            super::super::direct_git::parse_upload_pack_intent(&shallow_body).unwrap();
        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &shallow_intent,
                &comparison,
            )
            .await
            .expect("shallow blobless read-through should hydrate the repo");

        let shallow_file = repo_dir.join("shallow");
        assert!(
            shallow_file.exists(),
            "depth-limited hydration should leave the cache repo shallow"
        );
        assert!(
            !materializer.commit_exists(&repo_dir, &parent).await,
            "depth-limited hydration should not import the parent commit"
        );
        assert!(
            !materializer
                .prepare_upload_pack_from_cache(
                    &fixture.repo,
                    &Bytes::from(make_full_body(&commit))
                )
                .await
                .unwrap(),
            "shallow repos must decline full-history cache prepare"
        );

        let full_intent =
            super::super::direct_git::parse_upload_pack_intent(&make_full_body(&commit)).unwrap();
        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &full_intent,
                &comparison,
            )
            .await
            .expect("full-history read-through against a shallow repo should unshallow");

        assert!(
            !shallow_file.exists(),
            "full-history read-through should remove the shallow boundary"
        );
        assert!(
            materializer.commit_exists(&repo_dir, &parent).await,
            "unshallowed repo should contain the full commit ancestry"
        );
    }

    fn make_full_body(commit: &CommitSha) -> Vec<u8> {
        let mut body = make_upload_pack_pkt_line(&format!("want {commit} multi_ack\n"));
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("done\n"));
        body
    }

    #[tokio::test]
    async fn direct_want_falls_back_to_exact_sha_when_advertised_branch_moves_before_post() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));
        let advertised_commit = fixture.commit_and_push("advertised");
        let moved_commit = fixture.commit_and_push("moved");
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();

        let stale_comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), advertised_commit.to_string())]),
        };
        let mut body =
            make_upload_pack_pkt_line(&format!("want {advertised_commit} multi_ack thin-pack\n"));
        body.extend_from_slice(b"0000");
        body.extend(make_upload_pack_pkt_line("deepen 1\n"));
        body.extend(make_upload_pack_pkt_line("filter blob:none\n"));
        body.extend(make_upload_pack_pkt_line("done\n"));
        let intent = super::super::direct_git::parse_upload_pack_intent(&body).unwrap();

        materializer
            .ensure_upload_pack_intent_available_from_comparison(
                &fixture.repo,
                &intent,
                &stale_comparison,
            )
            .await
            .expect("stale advertised-ref fetch should fall back to exact SHA fetch");

        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &advertised_commit)
                .await,
            "the originally advertised wanted commit should be fetched exactly"
        );
        assert_eq!(
            state
                .git
                .rev_parse(&repo_dir, "refs/cache/upstream/heads/main")
                .await
                .unwrap(),
            moved_commit.to_string(),
            "the ref fetch may still record the newer branch tip"
        );
    }

    #[tokio::test]
    async fn anonymous_direct_want_hydrates_public_ref_manifest_on_post() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, manifest.generation).await;

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        stdfs::remove_dir_all(&repo_dir).unwrap();
        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), cached.commit.to_string())]),
        };

        let result = materializer
            .ensure_wants_available_from_comparison(
                &fixture.repo,
                &[cached.commit.to_string()],
                &comparison,
                false,
            )
            .await;

        assert!(
            result.is_ok(),
            "direct Git POST should hydrate known generation manifests while reading through: {result:?}"
        );

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &cached.commit)
                .await,
            "direct Git POST should hydrate public ref manifests"
        );

        let result = materializer
            .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
            .await;

        assert!(
            result.is_ok(),
            "restored public refs should remain directly available after hydration: {result:?}"
        );
    }

    #[tokio::test]
    async fn anonymous_direct_want_skips_manifest_restore_when_ref_is_already_hot() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, manifest.generation).await;

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert_eq!(
            state
                .git
                .rev_parse(&repo_dir, "refs/heads/main")
                .await
                .unwrap(),
            cached.commit.to_string()
        );
        assert!(
            materializer
                .commit_ready_for_serving(&repo_dir, &cached.commit)
                .await
        );

        delete_generation_packs(&state, &fixture.repo, manifest.generation).await;
        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        let comparison = UpstreamRefComparison {
            default_branch: Some("main".to_string()),
            all_upstream: HashMap::from([("main".to_string(), cached.commit.to_string())]),
        };

        materializer
            .ensure_wants_available_from_comparison(
                &fixture.repo,
                &[cached.commit.to_string()],
                &comparison,
                false,
            )
            .await
            .expect("hot public refs should not rehydrate the already-present manifest commit");
    }

    #[tokio::test]
    async fn public_ref_manifest_restore_seeds_hidden_base_without_public_ref() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, manifest.generation).await;

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        stdfs::remove_dir_all(&repo_dir).unwrap();
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        let branch = BranchName::parse("main").unwrap();

        assert_eq!(
            materializer
                .restore_upstream_ref_base_from_manifest(&fixture.repo, &repo_dir, &branch)
                .await
                .unwrap(),
            Some(cached.commit.clone())
        );
        assert_eq!(
            state
                .git
                .rev_parse(&repo_dir, "refs/cache/upstream/heads/main")
                .await
                .unwrap(),
            cached.commit.to_string()
        );
        assert!(
            state
                .git
                .rev_parse(&repo_dir, "refs/heads/main")
                .await
                .is_err(),
            "stale-base restore must not publish public serving refs"
        );
    }

    #[tokio::test]
    async fn authenticated_direct_want_uses_local_readiness_after_repo_authorization() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();

        stdfs::rename(
            fixture.upstream_path(),
            fixture.tmp.path().join("offline.git"),
        )
        .unwrap();

        let auth = UpstreamAuth::parse_header("Basic dXNlcjpwYXNz").unwrap();
        materializer
            .using_upstream_auth(&auth)
            .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
            .await
            .expect("repo authorization is sufficient; POST should check local readiness only");
    }

    #[tokio::test]
    async fn anonymous_direct_want_does_not_fetch_unrequested_changed_refs() {
        let fixture = GitFixture::new();
        let state = Arc::new(fixture.state());
        let materializer = Materializer::new(Arc::clone(&state));

        let cached = materializer
            .materialize(MaterializeRequest {
                repo: fixture.repo.clone(),
                selector: Selector::Branch(BranchName::parse("main").unwrap()),
                upstream_authorization: Default::default(),
            })
            .await
            .unwrap();
        let cached_manifest = wait_for_commit_manifest(&state, &fixture.repo, &cached.commit).await;
        wait_for_verified_generation(&state, &fixture.repo, cached_manifest.generation).await;

        stdfs::write(fixture.work_path().join("side.txt"), "side\n").unwrap();
        run_git(&fixture.work_path(), ["add", "side.txt"]);
        run_git(&fixture.work_path(), ["commit", "-m", "side branch"]);
        let side_commit =
            CommitSha::parse(git_stdout(&fixture.work_path(), ["rev-parse", "HEAD"])).unwrap();
        fixture.push_head_to_branch("side");

        let generation_keys_before = generation_object_keys(&state, &fixture.repo).await;

        materializer
            .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
            .await
            .expect("cached advertised tip should be served without fetching unrelated refs");

        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        assert!(
            state
                .git
                .rev_parse(&repo_dir, "refs/heads/side")
                .await
                .is_err(),
            "unrequested changed side branch should not be fetched"
        );
        assert!(
            materializer
                .get_commit_manifest(&fixture.repo, &side_commit)
                .await
                .unwrap()
                .is_none(),
            "unrequested side branch should not be published"
        );
        assert_eq!(
            generation_keys_before,
            generation_object_keys(&state, &fixture.repo).await,
            "serving an already-cached want should not write new generation objects"
        );
    }

    #[tokio::test]
    async fn anonymous_direct_want_allows_cached_public_blob() {
        let fixture = GitFixture::new();
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

        let blob = CommitSha::parse(git_stdout(
            &fixture.work_path(),
            ["rev-parse", "HEAD:README.md"],
        ))
        .unwrap();
        let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
        let object_types = state
            .git
            .cat_file_batch_types(&repo_dir, std::slice::from_ref(&blob))
            .await
            .unwrap();
        assert_eq!(object_types.get(&blob).map(String::as_str), Some("blob"));

        materializer
            .ensure_wants_available(&fixture.repo, &[blob.to_string()])
            .await
            .expect("anonymous wants should allow blobs reachable from public upstream refs");
    }

    #[test]
    fn synth_no_symref_when_default_branch_absent_from_refs() {
        let sha = "a".repeat(40);
        let comp = UpstreamRefComparison {
            all_upstream: HashMap::from([("feature".to_string(), sha.clone())]),
            default_branch: Some("main".to_string()),
        };
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // Capability line must exist (the \0 delimiter).
        assert!(
            text.contains('\0'),
            "capability line missing when default_branch is absent from refs"
        );

        // symref must NOT reference a branch that isn't in the advertisement.
        assert!(
            !text.contains("symref=HEAD:refs/heads/main"),
            "symref must not reference absent default_branch; got: {text}"
        );

        // The ref that IS present should still appear.
        assert!(text.contains("refs/heads/feature"));
        assert!(output.ends_with(b"0000"));
    }

    // ── Performance tests ────────────────────────────────────────────────

    #[test]
    fn synth_single_branch() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);

        let text = String::from_utf8_lossy(&output);
        assert!(text.contains(&format!("{sha} HEAD")));
        assert!(text.contains("refs/heads/main"));
        assert!(output.ends_with(b"0000"));
    }

    #[test]
    fn synth_multiple_branches_sorted() {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let sha_c = "c".repeat(40);
        let sha_d = "d".repeat(40);
        let sha_e = "e".repeat(40);
        let comp = make_comparison(
            &[
                ("zeta", &sha_e),
                ("alpha", &sha_a),
                ("main", &sha_c),
                ("beta", &sha_b),
                ("gamma", &sha_d),
            ],
            Some("main"),
        );
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // All branches should appear.
        for name in &["alpha", "beta", "gamma", "main", "zeta"] {
            assert!(
                text.contains(&format!("refs/heads/{name}")),
                "missing branch {name}"
            );
        }

        // Extract branch names from pkt-line data.
        // Each ref line is: "{sha} refs/heads/{name}\n" (no NUL separator).
        // The HEAD/capabilities line is: "{sha} HEAD\0{caps}\n" — skip it.
        let pkt_lines = parse_pkt_lines(&output);
        let mut branch_names: Vec<String> = Vec::new();
        for pkt in &pkt_lines {
            let line_str = String::from_utf8_lossy(pkt);
            // Skip capability lines (they contain NUL).
            if line_str.contains('\0') {
                continue;
            }
            if let Some(rest) = line_str.split("refs/heads/").nth(1) {
                let name = rest.trim().to_string();
                if !name.is_empty() {
                    branch_names.push(name);
                }
            }
        }
        let mut sorted = branch_names.clone();
        sorted.sort();
        assert_eq!(branch_names, sorted);
    }

    #[test]
    fn synth_no_default_branch_uses_first_sorted() {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let comp = make_comparison(&[("beta", &sha_b), ("alpha", &sha_a)], None);
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // First line should be the first sorted branch with capabilities.
        let lines = parse_pkt_lines(&output);
        let first_line = String::from_utf8_lossy(&lines[0]);
        assert!(
            first_line.contains("refs/heads/alpha"),
            "first line should be alpha (first sorted): {first_line}"
        );
        assert!(
            first_line.contains('\0'),
            "first line should contain capability separator"
        );

        assert!(text.contains("refs/heads/beta"));
    }

    #[test]
    fn synth_default_branch_not_in_refs() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("feature", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        // "main" is set as default but not in all_upstream. Should still
        // output feature and terminate.
        assert!(text.contains("refs/heads/feature"));
        assert!(output.ends_with(b"0000"));

        // BUG: When default_branch is set but absent from upstream refs,
        // the capability line (\0-delimited) is never emitted. Git clients
        // expect at least one ref line to carry capabilities. This assert
        // documents the bug — it will start passing once the source is fixed.
        // See: synthesize_ref_advertisement outer if-let vs else-if fallback.
        assert!(
            text.contains('\0'),
            "capability line missing when default_branch is absent from refs (known bug)"
        );
    }

    #[test]
    fn synth_pkt_line_length_correctness() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);

        let mut offset = 0;
        while offset + 4 <= output.len() {
            let hex = std::str::from_utf8(&output[offset..offset + 4]).unwrap();
            let len = usize::from_str_radix(hex, 16).unwrap();
            if len == 0 {
                offset += 4;
                continue;
            }
            assert!(
                len >= 4,
                "pkt-line at offset {offset} has invalid length {len}"
            );
            assert!(
                offset + len <= output.len(),
                "pkt-line at offset {offset} extends beyond data"
            );
            // Verify the 4-char hex prefix matches actual line length.
            let actual_data_len = len - 4;
            let actual_data = &output[offset + 4..offset + len];
            assert_eq!(
                actual_data.len(),
                actual_data_len,
                "pkt-line length mismatch"
            );
            offset += len;
        }
    }

    #[test]
    fn synth_capability_string_contents() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        for cap in &[
            "multi_ack",
            "thin-pack",
            "side-band-64k",
            "no-done",
            "filter",
            "object-format=sha1",
        ] {
            assert!(text.contains(cap), "missing capability: {cap}");
        }
    }

    #[test]
    fn synth_symref_capability() {
        let sha = "a".repeat(40);
        let comp = make_comparison(&[("main", &sha)], Some("main"));
        let output = synthesize_ref_advertisement(&comp);
        let text = String::from_utf8_lossy(&output);

        assert!(
            text.contains("symref=HEAD:refs/heads/main"),
            "missing symref capability"
        );
    }

    #[test]
    fn synth_empty_refs() {
        let comp = make_comparison(&[], None);
        let output = synthesize_ref_advertisement(&comp);
        assert_eq!(output, b"0000");
    }
}
