use super::*;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());
const ENV_KEYS: &[&str] = &[
    "GIT_CACHE_CONFIG",
    "GIT_CACHE_BIND_ADDR",
    "GIT_CACHE_ROOT",
    "GIT_CACHE_OBJECT_STORE_KIND",
    "GIT_CACHE_OBJECT_STORE_ROOT",
    "GIT_CACHE_OBJECT_STORE_BUCKET",
    "GIT_CACHE_OBJECT_STORE_PREFIX",
    "GIT_CACHE_OBJECT_STORE_ENDPOINT",
    "GIT_CACHE_S3_BUCKET",
    "GIT_CACHE_S3_PREFIX",
    "GIT_CACHE_S3_ENDPOINT",
    "GIT_CACHE_UPSTREAM_ROOT",
    "GIT_CACHE_GIT_BINARY",
    "GIT_CACHE_GIT_TIMEOUT_SECONDS",
    "GIT_CACHE_MAX_GIT_OUTPUT_BYTES",
    "GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV",
    "GIT_CACHE_RATE_LIMIT_PER_MINUTE",
    "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS",
    "GIT_CACHE_DISK_QUOTA_BYTES",
    "GIT_CACHE_DISK_MIN_FREE_BYTES",
    "GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH",
    "GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY",
    "GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT",
    "GIT_CACHE_GIT_REMOTE_PROXY_TEE_IMPORT",
    "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD",
    "GIT_CACHE_COMPACTION_INLINE",
    "GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES",
    "GIT_CACHE_ASYNC_MATERIALIZE_CONCURRENCY",
];

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    old: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    fn new(vars: &[(&str, &str)]) -> Self {
        let lock = ENV_LOCK.lock().unwrap();
        let old = ENV_KEYS
            .iter()
            .map(|key| (*key, env::var(key).ok()))
            .collect();
        for key in ENV_KEYS {
            env::remove_var(key);
        }
        for (key, value) in vars {
            env::set_var(key, value);
        }
        Self { _lock: lock, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for key in ENV_KEYS {
            env::remove_var(key);
        }
        for (key, value) in &self.old {
            if let Some(value) = value {
                env::set_var(key, value);
            }
        }
    }
}

#[test]
fn from_path_parses_valid_toml() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    write!(
        tmp,
        r#"
bind_addr = "127.0.0.1:9090"
cache_root = "/tmp/cache"
git_timeout_seconds = 60
rate_limit_per_minute = 500

[object_store]
kind = "local"
root = "/tmp/objects"

[disk]
quota_bytes = 5368709120
min_free_bytes = 1073741824
"#
    )
    .unwrap();

    let config = AppConfig::from_path(tmp.path()).unwrap();
    assert_eq!(config.bind_addr.port(), 9090);
    assert_eq!(config.cache_root, PathBuf::from("/tmp/cache"));
    assert_eq!(config.git_timeout_seconds, 60);
    assert_eq!(config.rate_limit_per_minute, 500);
}

#[test]
fn from_path_uses_defaults_for_omitted_fields() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    write!(
        tmp,
        r#"
bind_addr = "0.0.0.0:8080"
cache_root = "/cache"

[object_store]
kind = "local"
root = "/objects"

[disk]
quota_bytes = 1000000
min_free_bytes = 100000
"#
    )
    .unwrap();

    let config = AppConfig::from_path(tmp.path()).unwrap();
    assert_eq!(config.git_binary, PathBuf::from("git"));
    assert_eq!(config.git_timeout_seconds, 120);
    assert_eq!(config.rate_limit_per_minute, 120);
    assert_eq!(config.max_git_output_bytes, 16 * 1024 * 1024);
    assert_eq!(config.git_remote, GitRemoteConfig::default());
    assert_eq!(config.compaction, CompactionConfig::default());
    assert_eq!(config.async_materialize_concurrency, 8);
}

#[test]
fn compaction_config_default_values() {
    let config = CompactionConfig::default();
    assert_eq!(config.chain_depth_threshold, 10);
    assert!(!config.inline);
    assert_eq!(config.retention_secs, 24 * 60 * 60);
}

#[test]
fn git_remote_config_default_values() {
    let config = GitRemoteConfig::default();
    assert!(config.commit_read_through);
    assert_eq!(config.background_import_concurrency, 1);
    assert!(config.proxy_on_miss_by_default);
    assert!(config.proxy_tee_import);
}

#[test]
fn git_remote_config_serde_round_trip() {
    let config = GitRemoteConfig {
        commit_read_through: false,
        background_import_concurrency: 2,
        proxy_on_miss_by_default: false,
        proxy_tee_import: false,
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: GitRemoteConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(config, parsed);
}

#[test]
fn from_env_configures_s3_object_store_and_git_remote() {
    let _env = EnvGuard::new(&[
        ("GIT_CACHE_BIND_ADDR", "0.0.0.0:8080"),
        ("GIT_CACHE_ROOT", "/cache"),
        ("GIT_CACHE_OBJECT_STORE_KIND", "s3"),
        ("GIT_CACHE_S3_BUCKET", "git-cache-bucket"),
        ("GIT_CACHE_S3_PREFIX", "prod"),
        ("GIT_CACHE_S3_ENDPOINT", "https://s3.example.com"),
        ("GIT_CACHE_ALLOWED_UPSTREAM_HOSTS", "github.com, gitlab.com"),
        ("GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH", "off"),
        ("GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY", "4"),
        ("GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT", "off"),
        ("GIT_CACHE_GIT_REMOTE_PROXY_TEE_IMPORT", "off"),
        ("GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "4"),
        ("GIT_CACHE_COMPACTION_INLINE", "yes"),
        ("GIT_CACHE_COMPACTION_RETENTION_SECS", "3600"),
    ]);

    let config = AppConfig::from_env().unwrap();
    assert_eq!(config.bind_addr.port(), 8080);
    assert_eq!(config.cache_root, PathBuf::from("/cache"));
    assert_eq!(
        config.allowed_upstream_hosts,
        vec!["github.com".to_string(), "gitlab.com".to_string()]
    );
    assert!(!config.git_remote.commit_read_through);
    assert_eq!(config.git_remote.background_import_concurrency, 4);
    assert!(!config.git_remote.proxy_on_miss_by_default);
    assert!(!config.git_remote.proxy_tee_import);
    assert_eq!(config.compaction.chain_depth_threshold, 4);
    assert!(config.compaction.inline);
    assert_eq!(config.compaction.retention_secs, 3600);

    match config.object_store {
        ObjectStoreConfig::S3 {
            bucket,
            prefix,
            endpoint,
        } => {
            assert_eq!(bucket, "git-cache-bucket");
            assert_eq!(prefix, "prod");
            assert_eq!(endpoint.as_deref(), Some("https://s3.example.com"));
        }
        ObjectStoreConfig::Local { .. } => panic!("expected s3 object store"),
    }
}

#[test]
fn from_env_rejects_s3_without_bucket() {
    let _env = EnvGuard::new(&[("GIT_CACHE_OBJECT_STORE_KIND", "s3")]);
    assert!(AppConfig::from_env().is_err());
}

#[test]
fn from_path_rejects_invalid_toml() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    write!(tmp, "this is not valid toml ===").unwrap();
    assert!(AppConfig::from_path(tmp.path()).is_err());
}

#[test]
fn default_allowed_hosts_includes_github() {
    let hosts = default_allowed_upstream_hosts();
    assert!(hosts.contains(&"github.com".to_string()));
}
