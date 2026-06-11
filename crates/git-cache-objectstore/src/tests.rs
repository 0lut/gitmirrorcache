mod tests {
    use crate::{
        commit_manifest_key, generation_manifest_key, pack_key, read_commit_manifest,
        read_generation_manifest, read_ref_manifest, read_repo_generation_head, ref_manifest_key,
        repo_generation_head_key, validate_key, write_generation_manifest,
        write_generation_manifest_if_absent_or_matches, write_json_if_absent_or_matches,
        write_repo_generation_head, write_repo_generation_head_if_version_matches,
        GenerationPublish, LocalObjectStore, ObjectStore, PublishManifests,
    };
    use bytes::Bytes;
    use chrono::{DateTime, Utc};
    use git_cache_core::{
        CommitManifest, CommitSha, GenerationId, GenerationManifest, PackInfo, PackKind,
        RefManifest, RepoGenerationHead, RepoKey,
    };
    use std::path::Path;
    use tokio::fs;

    #[cfg(feature = "s3")]
    use crate::S3ObjectStore;
    #[cfg(feature = "s3")]
    use aws_credential_types::Credentials;
    #[cfg(feature = "s3")]
    use aws_sdk_s3::config::BehaviorVersion;
    #[cfg(feature = "s3")]
    use aws_sdk_s3::config::RequestChecksumCalculation;
    #[cfg(feature = "s3")]
    use aws_sdk_s3::Client;
    #[cfg(feature = "s3")]
    use aws_types::region::Region;

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "git-cache-objectstore-test-{}",
            uuid::Uuid::now_v7()
        ))
    }

    fn repo() -> RepoKey {
        RepoKey::parse("github.com/org/repo").unwrap()
    }

    fn commit(byte: char) -> CommitSha {
        CommitSha::parse(byte.to_string().repeat(40)).unwrap()
    }

    fn ts(seconds: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("2026-05-18T00:00:{seconds:02}Z"))
            .unwrap()
            .with_timezone(&Utc)
    }

    fn test_generation_id() -> GenerationId {
        GenerationId::new()
    }

    const PACK_BYTES: &[u8] = b"pack-bytes";
    const PACK_SHA256: &str = "4e03c5e500d33132d9bda1452f82e2258acfa7ff8e45146796010a89f34cd081";

    fn generation_manifest(repo: &RepoKey) -> GenerationManifest {
        let gen = test_generation_id();
        GenerationManifest {
            repo: repo.clone(),
            generation: gen,
            created_at: ts(1),
            verified_at: Some(ts(1)),
            packs: vec![PackInfo {
                key: pack_key(repo, PACK_SHA256).unwrap(),
                len: PACK_BYTES.len() as u64,
                sha256: PACK_SHA256.into(),
                kind: PackKind::Base,
            }],
            refs: Default::default(),
            head_ref: None,
            commits: vec![commit('a')],
        }
    }

    fn commit_manifest_with_gen(repo: &RepoKey, gen: GenerationId) -> CommitManifest {
        CommitManifest {
            repo: repo.clone(),
            commit: commit('a'),
            generation: gen,
            complete: true,
            verified_at: ts(2),
        }
    }

    fn ref_manifest_with_gen(repo: &RepoKey, gen: GenerationId) -> RefManifest {
        RefManifest {
            repo: repo.clone(),
            ref_name: "refs/heads/feature/cache".into(),
            commit: commit('a'),
            generation: gen,
            verified_at: ts(3),
        }
    }

    #[tokio::test]
    async fn put_if_version_matches_swaps_when_version_is_current() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store.put("cas/item.json", Bytes::from("v1")).await.unwrap();
        let (value, version) = store.get_versioned("cas/item.json").await.unwrap().unwrap();
        assert_eq!(value, Bytes::from("v1"));

        assert!(store
            .put_if_version_matches("cas/item.json", Bytes::from("v2"), &version)
            .await
            .unwrap());
        assert_eq!(
            store.get("cas/item.json").await.unwrap().unwrap(),
            Bytes::from("v2")
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn put_if_version_matches_rejects_stale_version() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store.put("cas/item.json", Bytes::from("v1")).await.unwrap();
        let (_, stale) = store.get_versioned("cas/item.json").await.unwrap().unwrap();
        store
            .put("cas/item.json", Bytes::from("concurrent"))
            .await
            .unwrap();

        assert!(!store
            .put_if_version_matches("cas/item.json", Bytes::from("v2"), &stale)
            .await
            .unwrap());
        assert_eq!(
            store.get("cas/item.json").await.unwrap().unwrap(),
            Bytes::from("concurrent")
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn put_if_version_matches_missing_object_returns_false() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store.put("cas/item.json", Bytes::from("v1")).await.unwrap();
        let (_, version) = store.get_versioned("cas/item.json").await.unwrap().unwrap();
        store.delete("cas/item.json").await.unwrap();

        assert!(!store
            .put_if_version_matches("cas/item.json", Bytes::from("v2"), &version)
            .await
            .unwrap());
        assert!(store.get("cas/item.json").await.unwrap().is_none());

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn repo_head_cas_first_write_then_stale_version_rejected() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);
        let repo = repo();

        let first = RepoGenerationHead {
            repo: repo.clone(),
            generation: test_generation_id(),
            tip_commits: vec![commit('a')],
            updated_at: ts(1),
        };
        assert!(
            write_repo_generation_head_if_version_matches(&store, &first, None)
                .await
                .unwrap()
        );
        assert!(
            !write_repo_generation_head_if_version_matches(&store, &first, None)
                .await
                .unwrap()
        );

        let (read_back, version) = crate::read_repo_generation_head_versioned(&store, &repo)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_back, first);

        let second = RepoGenerationHead {
            repo: repo.clone(),
            generation: test_generation_id(),
            tip_commits: vec![commit('b')],
            updated_at: ts(2),
        };
        assert!(
            write_repo_generation_head_if_version_matches(&store, &second, Some(&version))
                .await
                .unwrap()
        );
        assert!(
            !write_repo_generation_head_if_version_matches(&store, &first, Some(&version))
                .await
                .unwrap()
        );
        assert_eq!(
            read_repo_generation_head(&store, &repo)
                .await
                .unwrap()
                .unwrap(),
            second
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn delete_existing_key_removes_it() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store
            .put("data/item.json", Bytes::from("content"))
            .await
            .unwrap();
        assert!(store.exists("data/item.json").await.unwrap());

        store.delete("data/item.json").await.unwrap();
        assert!(!store.exists("data/item.json").await.unwrap());

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn delete_nonexistent_key_is_noop() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store.delete("does/not/exist.json").await.unwrap();

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn list_prefix_returns_matching_keys() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store.put("items/a.json", Bytes::from("a")).await.unwrap();
        store.put("items/b.json", Bytes::from("b")).await.unwrap();
        store
            .put("items/sub/c.json", Bytes::from("c"))
            .await
            .unwrap();
        store.put("other/d.json", Bytes::from("d")).await.unwrap();

        let mut keys = store.list_prefix("items/", None).await.unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec!["items/a.json", "items/b.json", "items/sub/c.json",]
        );

        let empty = store.list_prefix("nonexistent/", None).await.unwrap();
        assert!(empty.is_empty());

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn put_if_absent_is_conditional() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        assert!(store
            .put_if_absent("leases/update.json", Bytes::from("one"))
            .await
            .unwrap());
        assert!(!store
            .put_if_absent("leases/update.json", Bytes::from("two"))
            .await
            .unwrap());
        assert_eq!(
            store.get("leases/update.json").await.unwrap().unwrap(),
            Bytes::from("one")
        );

        let repo = repo();
        let generation = generation_manifest(&repo);
        assert!(
            write_generation_manifest_if_absent_or_matches(&store, &generation)
                .await
                .unwrap()
        );
        assert!(
            !write_generation_manifest_if_absent_or_matches(&store, &generation)
                .await
                .unwrap()
        );

        let mut conflicting = generation.clone();
        conflicting.commits = vec![commit('b')];
        assert!(
            write_generation_manifest_if_absent_or_matches(&store, &conflicting)
                .await
                .is_err()
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn manifests_round_trip_as_json() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);
        let repo = repo();

        let generation = generation_manifest(&repo);
        let commit = commit_manifest_with_gen(&repo, generation.generation);
        let reference = ref_manifest_with_gen(&repo, generation.generation);

        write_generation_manifest(&store, &generation)
            .await
            .unwrap();
        crate::write_commit_manifest(&store, &commit).await.unwrap();
        crate::write_ref_manifest(&store, &reference).await.unwrap();

        assert_eq!(
            read_generation_manifest(&store, &repo, generation.generation)
                .await
                .unwrap(),
            Some(generation.clone())
        );
        assert_eq!(
            read_commit_manifest(&store, &repo, &commit.commit)
                .await
                .unwrap(),
            Some(commit)
        );
        assert_eq!(
            read_ref_manifest(&store, &repo, &reference.ref_name)
                .await
                .unwrap(),
            Some(reference)
        );

        let head = RepoGenerationHead {
            repo: repo.clone(),
            generation: generation.generation,
            tip_commits: vec![self::commit('a'), self::commit('b')],
            updated_at: ts(6),
        };
        write_repo_generation_head(&store, &head).await.unwrap();
        assert_eq!(
            repo_generation_head_key(&repo),
            "repos/github.com/org/repo/manifests/generation-head.json"
        );
        assert_eq!(
            read_repo_generation_head(&store, &repo).await.unwrap(),
            Some(head)
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn publish_writes_packs_generation_then_manifests() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);
        let repo = repo();
        fs::create_dir_all(&root).await.unwrap();

        let generation = generation_manifest(&repo);
        let pack_store_key = generation.packs[0].key.clone();
        let commit_manifest = commit_manifest_with_gen(&repo, generation.generation);
        let reference = ref_manifest_with_gen(&repo, generation.generation);
        let publish = GenerationPublish::with_manifests(
            generation.clone(),
            PublishManifests {
                commits: vec![commit_manifest.clone()],
                refs: vec![reference.clone()],
            },
        );

        assert!(publish
            .publish_pack_files(
                &store,
                &[(pack_store_key.clone(), root.join("missing.pack"))]
            )
            .await
            .is_err());
        assert!(!store
            .exists(&generation_manifest_key(&repo, generation.generation))
            .await
            .unwrap());

        let pack_path = root.join("local.pack");
        fs::write(&pack_path, PACK_BYTES).await.unwrap();
        publish
            .publish_pack_files(&store, &[(pack_store_key.clone(), pack_path.clone())])
            .await
            .unwrap();
        assert_eq!(
            store.get(&pack_store_key).await.unwrap().unwrap(),
            Bytes::from_static(PACK_BYTES)
        );
        assert_eq!(
            read_generation_manifest(&store, &repo, generation.generation)
                .await
                .unwrap(),
            Some(generation.clone())
        );
        assert_eq!(
            read_commit_manifest(&store, &repo, &commit_manifest.commit)
                .await
                .unwrap(),
            Some(commit_manifest.clone())
        );
        assert_eq!(
            read_ref_manifest(&store, &repo, &reference.ref_name)
                .await
                .unwrap(),
            Some(reference.clone())
        );

        publish
            .publish_pack_files(&store, &[(pack_store_key.clone(), pack_path)])
            .await
            .unwrap();

        let mut changed_ref = reference;
        changed_ref.commit = commit('b');
        let changed_key = ref_manifest_key(&changed_ref.repo, &changed_ref.ref_name).unwrap();
        assert!(
            write_json_if_absent_or_matches(&store, &changed_key, &changed_ref)
                .await
                .is_err()
        );

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn rejects_traversal_keys() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);
        let repo = repo();

        for key in [
            "../secret",
            "/secret",
            "repo\\secret",
            "repo/./secret",
            "repo/../secret",
            "repo/",
        ] {
            assert!(validate_key(key).is_err(), "{key} should be rejected");
            assert!(store.put(key, Bytes::from_static(b"bad")).await.is_err());
        }

        assert!(ref_manifest_key(&repo, "refs/heads/../main").is_err());
        assert!(ref_manifest_key(&repo, "/refs/heads/main").is_err());

        assert_eq!(
            ref_manifest_key(&repo, "refs/heads/feature/cache").unwrap(),
            "repos/github.com/org/repo/manifests/refs/heads/feature%2Fcache.json"
        );
        assert_eq!(
            commit_manifest_key(&repo, &commit('a')),
            format!(
                "repos/github.com/org/repo/manifests/commits/aa/{}.json",
                "a".repeat(40)
            )
        );
        let _ = fs::remove_dir_all(root).await;
    }

    // ── Additional object store key validation tests ─────────────────

    #[test]
    fn validate_key_accepts_valid_relative_paths() {
        assert!(validate_key("repos/github.com/org/repo/manifest.json").is_ok());
        assert!(validate_key("a/b/c").is_ok());
        assert!(validate_key("simple.txt").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        assert!(validate_key("").is_err());
    }

    #[test]
    fn validate_key_rejects_leading_slash() {
        assert!(validate_key("/repos/test").is_err());
    }

    #[test]
    fn validate_key_rejects_trailing_slash() {
        assert!(validate_key("repos/test/").is_err());
    }

    #[test]
    fn validate_key_rejects_backslash() {
        assert!(validate_key("repos\\test").is_err());
    }

    #[test]
    fn validate_key_rejects_nul_byte() {
        assert!(validate_key("repos/te\0st").is_err());
    }

    #[test]
    fn validate_key_rejects_dot_dot_segments() {
        assert!(validate_key("repos/../secret").is_err());
        assert!(validate_key("..").is_err());
    }

    #[test]
    fn validate_key_rejects_single_dot_segments() {
        assert!(validate_key("repos/./test").is_err());
        assert!(validate_key(".").is_err());
    }

    #[test]
    fn validate_key_rejects_double_slash_empty_segments() {
        assert!(validate_key("repos//test").is_err());
    }

    #[tokio::test]
    async fn put_if_absent_first_write_returns_true() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        let result = store
            .put_if_absent("unique/key.json", Bytes::from("data"))
            .await
            .unwrap();
        assert!(result);

        let _ = fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn put_if_absent_duplicate_returns_false() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store
            .put_if_absent("dup/key.json", Bytes::from("first"))
            .await
            .unwrap();
        let second = store
            .put_if_absent("dup/key.json", Bytes::from("second"))
            .await
            .unwrap();
        assert!(!second);

        let data = store.get("dup/key.json").await.unwrap().unwrap();
        assert_eq!(data, Bytes::from("first"));

        let _ = fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn put_file_stores_content_correctly() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).await.unwrap();
        let src_path = src_dir.join("input.bundle");
        fs::write(&src_path, b"bundle-content-12345").await.unwrap();

        store
            .put_file("bundles/test.bundle", &src_path)
            .await
            .unwrap();

        let stored = store.get("bundles/test.bundle").await.unwrap().unwrap();
        assert_eq!(stored.as_ref(), b"bundle-content-12345");

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn put_file_is_atomic_no_temp_files_left() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        let src_dir = root.join("src");
        fs::create_dir_all(&src_dir).await.unwrap();
        let src_path = src_dir.join("input.dat");
        fs::write(&src_path, b"atomic-write-data").await.unwrap();

        store.put_file("data/output.dat", &src_path).await.unwrap();

        let parent = root.join("data");
        let mut entries = fs::read_dir(&parent).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "temp file not cleaned up: {name}");
        }

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn put_file_syncs_temp_file_before_rename() {
        // Verify put_file calls fsync on the temp file before renaming,
        // matching the durability contract of write_temp_file/put.
        // Uses strace to trace syscalls and verify fsync occurs between
        // the copy (write) and the rename.
        let strace_path = which_strace();
        if strace_path.is_none() {
            eprintln!("skipping put_file_syncs_temp_file_before_rename: strace not found");
            return;
        }

        let test_bin = std::env::current_exe().unwrap();
        let root = temp_root();

        let src_dir = root.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let src_path = src_dir.join("input.bundle");
        std::fs::write(&src_path, b"fsync-test-payload-data").unwrap();

        // Run the put_file helper as a subprocess under strace so we can
        // inspect syscalls. The helper is invoked via --ignored test name
        // with env vars telling it what to do.
        let output = std::process::Command::new(strace_path.unwrap())
            .args([
                "-f",
                "-e",
                "trace=fsync,fdatasync,rename,renameat,renameat2",
                "-o",
                "/dev/stderr",
                &test_bin.to_string_lossy(),
                "--ignored",
                "--exact",
                "tests::tests::put_file_strace_helper",
            ])
            .env("PUT_FILE_TEST_ROOT", path_str(&root))
            .env("PUT_FILE_TEST_SRC", path_str(&src_path))
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);

        // Parse strace output: look for fsync/fdatasync BEFORE rename
        let lines: Vec<&str> = stderr.lines().collect();
        let mut saw_fsync = false;
        let mut saw_rename_after_fsync = false;
        for line in &lines {
            if line.contains("fsync(") || line.contains("fdatasync(") {
                saw_fsync = true;
            }
            if saw_fsync
                && (line.contains("rename(")
                    || line.contains("renameat(")
                    || line.contains("renameat2("))
            {
                saw_rename_after_fsync = true;
            }
        }

        assert!(
            saw_fsync,
            "put_file must call fsync/fdatasync on temp file before rename.\n\
             strace output:\n{stderr}"
        );
        assert!(
            saw_rename_after_fsync,
            "put_file must call rename AFTER fsync.\n\
             strace output:\n{stderr}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Helper test invoked by `put_file_syncs_temp_file_before_rename` under strace.
    /// Not meant to be run directly — it reads env vars set by the parent test.
    #[tokio::test]
    #[ignore]
    async fn put_file_strace_helper() {
        let root_str = std::env::var("PUT_FILE_TEST_ROOT").expect("PUT_FILE_TEST_ROOT");
        let src_str = std::env::var("PUT_FILE_TEST_SRC").expect("PUT_FILE_TEST_SRC");

        let store = LocalObjectStore::new(&root_str);
        store
            .put_file("strace/bundle.dat", Path::new(&src_str))
            .await
            .unwrap();
    }

    fn which_strace() -> Option<std::path::PathBuf> {
        std::process::Command::new("which")
            .arg("strace")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if path.is_empty() {
                        None
                    } else {
                        Some(std::path::PathBuf::from(path))
                    }
                } else {
                    None
                }
            })
    }

    fn path_str(p: &std::path::Path) -> &str {
        p.to_str().expect("non-UTF-8 path")
    }

    #[cfg(feature = "s3")]
    struct MinioFixture {
        store: S3ObjectStore,
        prefix: String,
    }

    #[cfg(feature = "s3")]
    impl MinioFixture {
        async fn new(label: &str) -> Option<Self> {
            if std::env::var("GIT_CACHE_S3_INTEGRATION").ok().as_deref() != Some("1") {
                return None;
            }

            let endpoint = std::env::var("GIT_CACHE_S3_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
            let bucket = std::env::var("GIT_CACHE_S3_BUCKET")
                .unwrap_or_else(|_| "gitmirrorcache-test".into());
            let access_key =
                std::env::var("GIT_CACHE_S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
            let secret_key =
                std::env::var("GIT_CACHE_S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
            let prefix = format!("tests/{label}/{}", uuid::Uuid::now_v7());
            let config = aws_sdk_s3::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(Region::new("us-east-1"))
                .endpoint_url(endpoint)
                .force_path_style(true)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    None,
                    None,
                    "minio-integration",
                ))
                .build();
            let client = Client::from_conf(config);
            client.create_bucket().bucket(&bucket).send().await.ok();
            let store = S3ObjectStore::new(client, bucket, &prefix).unwrap();
            Some(Self { store, prefix })
        }
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn minio_store_round_trips_objects_and_metadata() {
        let Some(fixture) = MinioFixture::new("round-trip").await else {
            eprintln!(
                "skipping minio_store_round_trips_objects_and_metadata: set GIT_CACHE_S3_INTEGRATION=1"
            );
            return;
        };
        let store = fixture.store;

        let key = "objects/round-trip/blob.bin";
        store
            .put(key, Bytes::from_static(b"hello-minio"))
            .await
            .unwrap();
        assert_eq!(
            store.get(key).await.unwrap().unwrap(),
            Bytes::from_static(b"hello-minio")
        );

        let meta = store.head(key).await.unwrap().unwrap();
        assert_eq!(meta.key, key);
        assert_eq!(meta.len, b"hello-minio".len() as u64);
        assert!(store.exists(key).await.unwrap());

        store.delete(key).await.unwrap();
        assert!(store.get(key).await.unwrap().is_none());
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn minio_put_if_absent_preserves_first_writer() {
        let Some(fixture) = MinioFixture::new("conditional").await else {
            eprintln!(
                "skipping minio_put_if_absent_preserves_first_writer: set GIT_CACHE_S3_INTEGRATION=1"
            );
            return;
        };
        let store = fixture.store;

        assert!(store.get("conditional/item.json").await.unwrap().is_none());
        assert!(store
            .put_if_absent("conditional/item.json", Bytes::from_static(b"first"))
            .await
            .unwrap());
        assert_eq!(
            store.get("conditional/item.json").await.unwrap().unwrap(),
            Bytes::from_static(b"first")
        );
        assert!(!store
            .put_if_absent("conditional/item.json", Bytes::from_static(b"second"))
            .await
            .unwrap());
        assert_eq!(
            store.get("conditional/item.json").await.unwrap().unwrap(),
            Bytes::from_static(b"first")
        );
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn minio_put_if_version_matches_detects_concurrent_update() {
        let Some(fixture) = MinioFixture::new("cas").await else {
            eprintln!(
                "skipping minio_put_if_version_matches_detects_concurrent_update: set GIT_CACHE_S3_INTEGRATION=1"
            );
            return;
        };
        let store = fixture.store;

        let key = "cas/item.json";
        store.put(key, Bytes::from_static(b"v1")).await.unwrap();
        let (value, version) = store.get_versioned(key).await.unwrap().unwrap();
        assert_eq!(value, Bytes::from_static(b"v1"));

        assert!(store
            .put_if_version_matches(key, Bytes::from_static(b"v2"), &version)
            .await
            .unwrap());
        assert_eq!(
            store.get(key).await.unwrap().unwrap(),
            Bytes::from_static(b"v2")
        );

        // The token from v1 is now stale; the swap must be refused.
        assert!(!store
            .put_if_version_matches(key, Bytes::from_static(b"v3"), &version)
            .await
            .unwrap());
        assert_eq!(
            store.get(key).await.unwrap().unwrap(),
            Bytes::from_static(b"v2")
        );

        store.delete(key).await.unwrap();
    }

    #[cfg(feature = "s3")]
    #[tokio::test]
    async fn minio_manifest_publish_and_listing_uses_prefix() {
        let Some(fixture) = MinioFixture::new("manifest").await else {
            eprintln!(
                "skipping minio_manifest_publish_and_listing_uses_prefix: set GIT_CACHE_S3_INTEGRATION=1"
            );
            return;
        };
        let store = fixture.store;
        let repo = repo();
        let generation = generation_manifest(&repo);
        let commit_manifest = commit_manifest_with_gen(&repo, generation.generation);
        let reference = ref_manifest_with_gen(&repo, generation.generation);

        let pack_path = std::env::temp_dir().join(format!("minio-pack-{}", uuid::Uuid::now_v7()));
        fs::write(&pack_path, PACK_BYTES).await.unwrap();
        GenerationPublish::with_manifests(
            generation.clone(),
            PublishManifests {
                commits: vec![commit_manifest.clone()],
                refs: vec![reference.clone()],
            },
        )
        .publish_pack_files(&store, &[(generation.packs[0].key.clone(), pack_path)])
        .await
        .unwrap();

        assert_eq!(
            store.get(&generation.packs[0].key).await.unwrap().unwrap(),
            Bytes::from_static(PACK_BYTES)
        );
        assert_eq!(
            read_generation_manifest(&store, &repo, generation.generation)
                .await
                .unwrap(),
            Some(generation.clone())
        );
        assert_eq!(
            read_commit_manifest(&store, &repo, &commit_manifest.commit)
                .await
                .unwrap(),
            Some(commit_manifest)
        );
        assert_eq!(
            read_ref_manifest(&store, &repo, &reference.ref_name)
                .await
                .unwrap(),
            Some(reference)
        );

        let keys = store
            .list_prefix(&format!("repos/{repo}/"), Some(2))
            .await
            .unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|key| !key.starts_with(&fixture.prefix)));
    }

    #[tokio::test]
    async fn head_returns_metadata_for_existing_key() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        store
            .put("meta/test.bin", Bytes::from("hello-metadata"))
            .await
            .unwrap();

        let meta = store.head("meta/test.bin").await.unwrap();
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert_eq!(meta.key, "meta/test.bin");
        assert_eq!(meta.len, 14); // "hello-metadata".len()
        assert!(meta.updated_at.is_some());

        let _ = fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn head_returns_none_for_missing_key() {
        let root = temp_root();
        let store = LocalObjectStore::new(&root);

        let meta = store.head("does/not/exist.bin").await.unwrap();
        assert!(meta.is_none());

        let _ = fs::remove_dir_all(root).await;
    }
}
