use super::*;

#[test]
fn local_store_root_gets_v2_suffix() {
    assert_eq!(
        v2_local_store_root(Path::new("/tmp/object-store")),
        PathBuf::from("/tmp/object-store-v3")
    );
    assert_eq!(
        v2_local_store_root(Path::new("/tmp/object-store-v3")),
        PathBuf::from("/tmp/object-store-v3")
    );
    assert_eq!(
        v2_local_store_root(Path::new("/")),
        PathBuf::from("/").join("v3")
    );
}

#[test]
fn s3_prefix_gets_v2_suffix() {
    assert_eq!(v2_s3_prefix("repos"), "repos-v3");
    assert_eq!(v2_s3_prefix("prod/repos"), "prod/repos-v3");
    assert_eq!(v2_s3_prefix("/prod/repos/"), "prod/repos-v3");
    assert_eq!(v2_s3_prefix("repos-v3"), "repos-v3");
    assert_eq!(v2_s3_prefix("prod/v3"), "prod/v3");
    assert_eq!(v2_s3_prefix(""), "v3");
}

#[test]
fn startup_cleans_stale_disk_reservations() {
    let root = tempfile::tempdir().expect("cache root");
    let objects = tempfile::tempdir().expect("object root");
    let disk = DiskManager::new(root.path(), 10_000, 0);
    let reservation = disk.reserve(1024).expect("reserve");
    let temp_path = reservation.temp_path();
    std::fs::write(temp_path.join("verification.tmp"), vec![0u8; 64]).expect("tmp");
    std::mem::forget(reservation);

    let mut config = test_config(root.path().to_path_buf(), objects.path().to_path_buf());
    config.disk.quota_bytes = 10_000;
    let state = AppState::try_new(config).expect("state");
    let status = state.disk.inner().status().expect("status");

    assert!(!temp_path.exists());
    assert_eq!(status.reserved_bytes, 0);
}

fn test_config(cache_root: PathBuf, object_root: PathBuf) -> AppConfig {
    AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        cache_root,
        upstream_root: None,
        git_binary: PathBuf::from("git"),
        git_timeout_seconds: 30,
        max_git_output_bytes: 1024 * 1024,
        object_store: ObjectStoreConfig::Local { root: object_root },
        upstream_auth_token_env: None,
        rate_limit_per_minute: 120,
        allowed_upstream_hosts: vec!["github.com".into()],
        disk: git_cache_core::DiskConfig {
            quota_bytes: 10_000,
            min_free_bytes: 0,
            access_flush_interval_secs: 60,
        },
        git_remote: Default::default(),
        compaction: Default::default(),
        lfs: Default::default(),
        shutdown: Default::default(),
        max_concurrent_git_processes: 1,
        async_materialize_concurrency: 2,
        public_path_prefix: String::new(),
        use_gitoxide: true,
    }
}
