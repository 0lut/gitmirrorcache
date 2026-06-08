use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

pub fn test_config(addr: SocketAddr, tmp: &Path) -> AppConfig {
    test_config_with_upstream(addr, tmp, tmp.join("upstreams"))
}

pub fn test_config_with_upstream(
    addr: SocketAddr,
    tmp: &Path,
    upstream_root: impl Into<PathBuf>,
) -> AppConfig {
    AppConfig {
        bind_addr: addr,
        cache_root: tmp.join("cache"),
        upstream_root: Some(upstream_root.into()),
        git_binary: PathBuf::from("git"),
        git_timeout_seconds: 120,
        max_git_output_bytes: 64 * 1024 * 1024,
        object_store: ObjectStoreConfig::Local {
            root: tmp.join("objects"),
        },
        upstream_auth_token_env: None,
        rate_limit_per_minute: 0,
        allowed_upstream_hosts: vec!["github.com".into()],
        disk: git_cache_core::DiskConfig {
            quota_bytes: 1024 * 1024 * 1024,
            min_free_bytes: 0,
        },
        git_remote: GitRemoteConfig {
            enabled: true,
            ..Default::default()
        },
        compaction: Default::default(),
        max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
        max_concurrent_generation_verifications: 1,
    }
}
