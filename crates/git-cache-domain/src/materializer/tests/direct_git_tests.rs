use super::*;

#[test]
fn synthesize_ref_advertisement_contains_head_and_refs() {
    let comparison = UpstreamRefComparison {
        changed: HashMap::new(),
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
        changed: HashMap::new(),
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
        changed: HashMap::new(),
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

#[tokio::test]
async fn anonymous_direct_want_rejects_locally_cached_unadvertised_commit() {
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

    let error = materializer
        .ensure_wants_available(&fixture.repo, &[private_commit.to_string()])
        .await
        .expect_err("anonymous wants must not trust locally cached private commits");

    assert!(
        matches!(
            error,
            GitCacheError::NotFound(_) | GitCacheError::Forbidden(_)
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn anonymous_direct_want_allows_cached_public_ancestor() {
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
    fixture.commit_and_push("public descendant");

    materializer
        .ensure_wants_available(&fixture.repo, &[first.commit.to_string()])
        .await
        .expect("anonymous wants should allow commits reachable from public upstream refs");
}

#[tokio::test]
async fn anonymous_direct_want_requires_upstream_proof_even_when_public_ref_is_cached() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let cached = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
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

    let result = materializer
        .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
        .await;

    assert!(
        matches!(result, Err(GitCacheError::UpstreamUnavailable(_))),
        "direct Git POST wants must not use cached public refs as fresh upstream proof: {result:?}"
    );
}

#[tokio::test]
async fn anonymous_direct_want_does_not_verify_pending_generation_on_post() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));
    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    let commit = fixture.head_commit();

    run_git(
        &repo_dir,
        [
            "fetch",
            "--no-tags",
            fixture.upstream_path().to_str().unwrap(),
            "+refs/heads/main:refs/cache/upstream/heads/main",
        ],
    );
    let generation = GenerationId::new();
    let reservation = state.disk.reserve(1024 * 1024 * 64).await.unwrap();
    let temp_path = reservation.temp_path().unwrap();
    fs::create_dir_all(&temp_path).await.unwrap();
    let bundle_path = temp_path.join("pending.bundle");
    state
        .git
        .bundle_create_all(&repo_dir, &bundle_path)
        .await
        .unwrap();

    let now = Utc::now();
    let generation_manifest = GenerationManifest {
        repo: fixture.repo.clone(),
        generation,
        bundle_key: bundle_key(&fixture.repo, generation),
        parent_generation: None,
        created_at: now,
        commits: vec![commit.clone()],
    };
    let publish_manifests = PublishManifests {
        commits: vec![CommitManifest {
            repo: fixture.repo.clone(),
            commit: commit.clone(),
            generation,
            complete: true,
            verified_at: now,
        }],
        refs: Vec::new(),
        sessions: Vec::new(),
    };
    let head = RepoGenerationHead {
        repo: fixture.repo.clone(),
        generation,
        tip_commits: vec![commit.clone()],
        updated_at: now,
    };
    GenerationPublish::with_manifests(generation_manifest, publish_manifests)
        .publish_pending_bundle_file(&*state.store, &bundle_path, head, None)
        .await
        .unwrap();
    reservation.release().await.unwrap();

    stdfs::remove_dir_all(&repo_dir).unwrap();
    stdfs::rename(
        fixture.upstream_path(),
        fixture.tmp.path().join("offline.git"),
    )
    .unwrap();

    let comparison = UpstreamRefComparison {
        changed: HashMap::new(),
        default_branch: Some("main".to_string()),
        all_upstream: HashMap::from([("main".to_string(), commit.to_string())]),
    };
    let result = materializer
        .ensure_wants_available_from_comparison(&fixture.repo, &[commit.to_string()], &comparison)
        .await;

    assert!(
        matches!(result, Err(GitCacheError::UpstreamUnavailable(_))),
        "direct Git POST should fetch proved refs instead of verifying pending generations: {result:?}"
    );

    let repo_dir = materializer.repo_dir(&fixture.repo);
    assert!(
        !materializer
            .commit_ready_for_serving(&repo_dir, &commit)
            .await,
        "direct Git POST must not hydrate pending generations"
    );
    assert!(
        materializer
            .get_commit_manifest(&fixture.repo, &commit)
            .await
            .unwrap()
            .is_none(),
        "direct Git POST must not verify and publish pending generations"
    );
}

#[tokio::test]
async fn anonymous_direct_want_does_not_hydrate_public_ref_manifest_on_post() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let cached = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
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
        changed: HashMap::new(),
        default_branch: Some("main".to_string()),
        all_upstream: HashMap::from([("main".to_string(), cached.commit.to_string())]),
    };

    let result = materializer
        .ensure_wants_available_from_comparison(
            &fixture.repo,
            &[cached.commit.to_string()],
            &comparison,
        )
        .await;

    assert!(
        matches!(result, Err(GitCacheError::UpstreamUnavailable(_))),
        "direct Git POST should fetch proved refs instead of hydrating generation manifests: {result:?}"
    );

    let repo_dir = materializer.ensure_repo_dir(&fixture.repo).await.unwrap();
    assert!(
        state
            .git
            .rev_parse(&repo_dir, "refs/cache/upstream/heads/main")
            .await
            .is_err(),
        "direct Git POST must not restore hidden refs by hydrating a manifest"
    );
    assert!(
        !materializer
            .commit_ready_for_serving(&repo_dir, &cached.commit)
            .await,
        "direct Git POST must not hydrate public ref manifests"
    );

    let result = materializer
        .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
        .await;

    assert!(
        matches!(result, Err(GitCacheError::UpstreamUnavailable(_))),
        "restored public refs are availability hints, not fresh direct-Git POST proof: {result:?}"
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
            mode: RequestMode::Strict,
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

    state
        .store
        .delete(&bundle_key(&fixture.repo, manifest.generation))
        .await
        .unwrap();
    stdfs::rename(
        fixture.upstream_path(),
        fixture.tmp.path().join("offline.git"),
    )
    .unwrap();

    let comparison = UpstreamRefComparison {
        changed: HashMap::new(),
        default_branch: Some("main".to_string()),
        all_upstream: HashMap::from([("main".to_string(), cached.commit.to_string())]),
    };

    materializer
        .ensure_wants_available_from_comparison(
            &fixture.repo,
            &[cached.commit.to_string()],
            &comparison,
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
            mode: RequestMode::Strict,
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
async fn authenticated_direct_want_requires_upstream_proof_after_repo_authorization() {
    let fixture = GitFixture::new();
    let state = Arc::new(fixture.state());
    let materializer = Materializer::new(Arc::clone(&state));

    let cached = materializer
        .materialize(MaterializeRequest {
            repo: fixture.repo.clone(),
            selector: Selector::Branch(BranchName::parse("main").unwrap()),
            mode: RequestMode::Strict,
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
    let result = materializer
        .using_upstream_auth(&auth)
        .ensure_wants_available(&fixture.repo, &[cached.commit.to_string()])
        .await;

    assert!(
        matches!(result, Err(GitCacheError::UpstreamUnavailable(_))),
        "repo authorization alone must not make cached objects valid direct-Git wants: {result:?}"
    );
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
            mode: RequestMode::Strict,
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
            mode: RequestMode::Strict,
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
        changed: HashMap::new(),
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
