use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub cache_root: PathBuf,
    #[serde(default)]
    pub upstream_root: Option<PathBuf>,
    #[serde(default = "default_git_binary")]
    pub git_binary: PathBuf,
    #[serde(default = "default_git_timeout_seconds")]
    pub git_timeout_seconds: u64,
    #[serde(default = "default_max_git_output_bytes")]
    pub max_git_output_bytes: usize,
    pub object_store: ObjectStoreConfig,
    #[serde(default)]
    pub upstream_auth_token_env: Option<String>,
    #[serde(default = "default_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
    #[serde(default = "default_allowed_upstream_hosts")]
    pub allowed_upstream_hosts: Vec<String>,
    pub disk: DiskConfig,
    #[serde(default)]
    pub git_remote: GitRemoteConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default = "default_max_concurrent_git_processes")]
    pub max_concurrent_git_processes: usize,
    #[serde(default = "default_async_materialize_concurrency")]
    pub async_materialize_concurrency: usize,
    /// Use in-process gitoxide for local read-only Git operations instead of
    /// spawning the `git` binary. Acts as a kill switch when disabled.
    #[serde(default = "default_use_gitoxide")]
    pub use_gitoxide: bool,
}

impl AppConfig {
    pub fn from_path(path: impl AsRef<Path>) -> crate::Result<Self> {
        let raw = fs::read_to_string(path)?;
        toml::from_str(&raw)
            .map_err(|err| crate::GitCacheError::Validation(format!("invalid config file: {err}")))
    }

    pub fn from_env() -> crate::Result<Self> {
        if let Ok(path) = env::var("GIT_CACHE_CONFIG") {
            return Self::from_path(path);
        }

        let bind_addr = env::var("GIT_CACHE_BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .map_err(|err| crate::GitCacheError::Validation(format!("invalid bind addr: {err}")))?;

        let cache_root =
            PathBuf::from(env::var("GIT_CACHE_ROOT").unwrap_or_else(|_| "./cache".into()));
        let object_store = object_store_from_env()?;
        let upstream_root = env::var("GIT_CACHE_UPSTREAM_ROOT").ok().map(PathBuf::from);
        let allowed_upstream_hosts = parse_csv_env(
            "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS",
            default_allowed_upstream_hosts(),
        );

        Ok(Self {
            bind_addr,
            cache_root,
            upstream_root,
            git_binary: PathBuf::from(
                env::var("GIT_CACHE_GIT_BINARY").unwrap_or_else(|_| "git".into()),
            ),
            git_timeout_seconds: parse_env(
                "GIT_CACHE_GIT_TIMEOUT_SECONDS",
                default_git_timeout_seconds(),
            )?,
            max_git_output_bytes: parse_env(
                "GIT_CACHE_MAX_GIT_OUTPUT_BYTES",
                default_max_git_output_bytes(),
            )?,
            object_store,
            upstream_auth_token_env: env::var("GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV").ok(),
            rate_limit_per_minute: parse_env(
                "GIT_CACHE_RATE_LIMIT_PER_MINUTE",
                default_rate_limit_per_minute(),
            )?,
            allowed_upstream_hosts,
            disk: DiskConfig {
                quota_bytes: parse_env("GIT_CACHE_DISK_QUOTA_BYTES", 10 * 1024 * 1024 * 1024)?,
                min_free_bytes: parse_env("GIT_CACHE_DISK_MIN_FREE_BYTES", 1024 * 1024 * 1024)?,
            },
            git_remote: GitRemoteConfig {
                enabled: parse_bool_env("GIT_CACHE_GIT_REMOTE_ENABLED", false)?,
                commit_read_through: parse_bool_env(
                    "GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH",
                    true,
                )?,
                background_import_concurrency: parse_env(
                    "GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY",
                    default_background_import_concurrency(),
                )?,
                proxy_on_miss_by_default: parse_bool_env(
                    "GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT",
                    true,
                )?,
            },
            compaction: CompactionConfig {
                chain_depth_threshold: parse_env(
                    "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD",
                    default_compaction_threshold(),
                )?,
                inline: parse_bool_env("GIT_CACHE_COMPACTION_INLINE", false)?,
            },
            max_concurrent_git_processes: parse_env(
                "GIT_CACHE_MAX_CONCURRENT_GIT_PROCESSES",
                default_max_concurrent_git_processes(),
            )?,
            async_materialize_concurrency: parse_env(
                "GIT_CACHE_ASYNC_MATERIALIZE_CONCURRENCY",
                default_async_materialize_concurrency(),
            )?,
            use_gitoxide: parse_bool_env("GIT_CACHE_USE_GITOXIDE", default_use_gitoxide())?,
        })
    }
}

pub const ENV_OBJECT_STORE_KIND: &str = "GIT_CACHE_OBJECT_STORE_KIND";
pub const ENV_OBJECT_STORE_ROOT: &str = "GIT_CACHE_OBJECT_STORE_ROOT";
pub const ENV_OBJECT_STORE_BUCKET: &str = "GIT_CACHE_OBJECT_STORE_BUCKET";
pub const ENV_OBJECT_STORE_PREFIX: &str = "GIT_CACHE_OBJECT_STORE_PREFIX";
pub const ENV_OBJECT_STORE_ENDPOINT: &str = "GIT_CACHE_OBJECT_STORE_ENDPOINT";
pub const ENV_S3_BUCKET: &str = "GIT_CACHE_S3_BUCKET";
pub const ENV_S3_PREFIX: &str = "GIT_CACHE_S3_PREFIX";
pub const ENV_S3_ENDPOINT: &str = "GIT_CACHE_S3_ENDPOINT";
pub const ENV_GCS_BUCKET: &str = "GIT_CACHE_GCS_BUCKET";
pub const ENV_GCS_PREFIX: &str = "GIT_CACHE_GCS_PREFIX";
pub const ENV_GCS_ENDPOINT: &str = "GIT_CACHE_GCS_ENDPOINT";

const DEFAULT_OBJECT_STORE_PREFIX: &str = "repos";
const DEFAULT_LOCAL_OBJECT_STORE_ROOT: &str = "./tmp/object-store";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjectStoreKind {
    Local,
    S3,
    Gcs,
}

impl ObjectStoreKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
            Self::Gcs => "gcs",
        }
    }

    fn from_env() -> crate::Result<Self> {
        let raw = env::var(ENV_OBJECT_STORE_KIND)
            .unwrap_or_else(|_| Self::Local.as_str().into())
            .to_ascii_lowercase();
        match raw.as_str() {
            "local" => Ok(Self::Local),
            "s3" => Ok(Self::S3),
            "gcs" => Ok(Self::Gcs),
            other => Err(crate::GitCacheError::Validation(format!(
                "unsupported {ENV_OBJECT_STORE_KIND} `{other}`"
            ))),
        }
    }
}

fn object_store_from_env() -> crate::Result<ObjectStoreConfig> {
    let kind = ObjectStoreKind::from_env()?;

    let s3_configured = env_present(ENV_S3_BUCKET);
    let gcs_configured = env_present(ENV_GCS_BUCKET);
    if s3_configured && gcs_configured {
        return Err(crate::GitCacheError::Validation(format!(
            "a deployment uses exactly one durable object-store backend: \
             both {ENV_S3_BUCKET} and {ENV_GCS_BUCKET} are set"
        )));
    }
    match (kind, s3_configured, gcs_configured) {
        (ObjectStoreKind::S3, _, true) => {
            return Err(kind_conflict_error(kind, ENV_GCS_BUCKET));
        }
        (ObjectStoreKind::Gcs, true, _) => {
            return Err(kind_conflict_error(kind, ENV_S3_BUCKET));
        }
        _ => {}
    }

    match kind {
        ObjectStoreKind::Local => Ok(ObjectStoreConfig::Local {
            root: PathBuf::from(
                env::var(ENV_OBJECT_STORE_ROOT)
                    .unwrap_or_else(|_| DEFAULT_LOCAL_OBJECT_STORE_ROOT.into()),
            ),
        }),
        ObjectStoreKind::S3 => {
            let (bucket, prefix, endpoint) =
                durable_backend_from_env(kind, ENV_S3_BUCKET, ENV_S3_PREFIX, ENV_S3_ENDPOINT)?;
            Ok(ObjectStoreConfig::S3 {
                bucket,
                prefix,
                endpoint,
            })
        }
        ObjectStoreKind::Gcs => {
            let (bucket, prefix, endpoint) =
                durable_backend_from_env(kind, ENV_GCS_BUCKET, ENV_GCS_PREFIX, ENV_GCS_ENDPOINT)?;
            Ok(ObjectStoreConfig::Gcs {
                bucket,
                prefix,
                endpoint,
            })
        }
    }
}

fn kind_conflict_error(
    kind: ObjectStoreKind,
    conflicting_bucket_env: &str,
) -> crate::GitCacheError {
    crate::GitCacheError::Validation(format!(
        "{ENV_OBJECT_STORE_KIND}={} conflicts with {conflicting_bucket_env}",
        kind.as_str()
    ))
}

fn durable_backend_from_env(
    kind: ObjectStoreKind,
    bucket_env: &str,
    prefix_env: &str,
    endpoint_env: &str,
) -> crate::Result<(String, String, Option<String>)> {
    let bucket = env::var(bucket_env)
        .or_else(|_| env::var(ENV_OBJECT_STORE_BUCKET))
        .map_err(|_| {
            crate::GitCacheError::Validation(format!(
                "{ENV_OBJECT_STORE_KIND}={} requires {bucket_env}",
                kind.as_str()
            ))
        })?;
    if bucket.trim().is_empty() {
        return Err(crate::GitCacheError::Validation(format!(
            "{bucket_env} must not be empty"
        )));
    }

    let prefix = env::var(prefix_env)
        .or_else(|_| env::var(ENV_OBJECT_STORE_PREFIX))
        .unwrap_or_else(|_| DEFAULT_OBJECT_STORE_PREFIX.into());
    let endpoint = env::var(endpoint_env)
        .or_else(|_| env::var(ENV_OBJECT_STORE_ENDPOINT))
        .ok()
        .filter(|value| !value.trim().is_empty());
    Ok((bucket, prefix, endpoint))
}

fn env_present(key: &str) -> bool {
    env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectStoreConfig {
    Local {
        root: PathBuf,
    },
    S3 {
        bucket: String,
        prefix: String,
        endpoint: Option<String>,
    },
    Gcs {
        bucket: String,
        prefix: String,
        endpoint: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskConfig {
    pub quota_bytes: u64,
    pub min_free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_compaction_threshold")]
    pub chain_depth_threshold: u32,
    #[serde(default)]
    pub inline: bool,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            chain_depth_threshold: default_compaction_threshold(),
            inline: false,
        }
    }
}

fn default_compaction_threshold() -> u32 {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub commit_read_through: bool,
    #[serde(default = "default_background_import_concurrency")]
    pub background_import_concurrency: usize,
    #[serde(default = "default_true")]
    pub proxy_on_miss_by_default: bool,
}

impl Default for GitRemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            commit_read_through: true,
            background_import_concurrency: default_background_import_concurrency(),
            proxy_on_miss_by_default: true,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_background_import_concurrency() -> usize {
    1
}

fn default_allowed_upstream_hosts() -> Vec<String> {
    vec!["github.com".to_string()]
}

fn default_git_binary() -> PathBuf {
    PathBuf::from("git")
}

fn default_git_timeout_seconds() -> u64 {
    120
}

fn default_max_git_output_bytes() -> usize {
    16 * 1024 * 1024
}

fn default_rate_limit_per_minute() -> u32 {
    120
}

pub fn default_max_concurrent_git_processes() -> usize {
    64
}

pub fn default_async_materialize_concurrency() -> usize {
    2
}

pub fn default_use_gitoxide() -> bool {
    true
}

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> crate::Result<T>
where
    T::Err: std::fmt::Display,
{
    match env::var(name) {
        Ok(value) => value.parse().map_err(|err| {
            crate::GitCacheError::Validation(format!("invalid {name} value `{value}`: {err}"))
        }),
        Err(_) => Ok(default),
    }
}

fn parse_bool_env(name: &str, default: bool) -> crate::Result<bool> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            _ => Err(crate::GitCacheError::Validation(format!(
                "invalid {name} value `{value}`: expected boolean"
            ))),
        },
        Err(_) => Ok(default),
    }
}

fn parse_csv_env(name: &str, default: Vec<String>) -> Vec<String> {
    let Ok(value) = env::var(name) else {
        return default;
    };
    let values: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect();
    if values.is_empty() {
        default
    } else {
        values
    }
}

#[cfg(test)]
mod tests {
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
        "GIT_CACHE_GCS_BUCKET",
        "GIT_CACHE_GCS_PREFIX",
        "GIT_CACHE_GCS_ENDPOINT",
        "GIT_CACHE_UPSTREAM_ROOT",
        "GIT_CACHE_GIT_BINARY",
        "GIT_CACHE_GIT_TIMEOUT_SECONDS",
        "GIT_CACHE_MAX_GIT_OUTPUT_BYTES",
        "GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV",
        "GIT_CACHE_RATE_LIMIT_PER_MINUTE",
        "GIT_CACHE_ALLOWED_UPSTREAM_HOSTS",
        "GIT_CACHE_DISK_QUOTA_BYTES",
        "GIT_CACHE_DISK_MIN_FREE_BYTES",
        "GIT_CACHE_GIT_REMOTE_ENABLED",
        "GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH",
        "GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY",
        "GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT",
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
        assert_eq!(config.async_materialize_concurrency, 2);
    }

    #[test]
    fn compaction_config_default_values() {
        let config = CompactionConfig::default();
        assert_eq!(config.chain_depth_threshold, 10);
        assert!(!config.inline);
    }

    #[test]
    fn git_remote_config_default_values() {
        let config = GitRemoteConfig::default();
        assert!(!config.enabled);
        assert!(config.commit_read_through);
        assert_eq!(config.background_import_concurrency, 1);
        assert!(config.proxy_on_miss_by_default);
    }

    #[test]
    fn git_remote_config_serde_round_trip() {
        let config = GitRemoteConfig {
            enabled: true,
            commit_read_through: false,
            background_import_concurrency: 2,
            proxy_on_miss_by_default: false,
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
            ("GIT_CACHE_GIT_REMOTE_ENABLED", "true"),
            ("GIT_CACHE_GIT_REMOTE_COMMIT_READ_THROUGH", "off"),
            ("GIT_CACHE_GIT_REMOTE_BACKGROUND_IMPORT_CONCURRENCY", "4"),
            ("GIT_CACHE_GIT_REMOTE_PROXY_ON_MISS_BY_DEFAULT", "off"),
            ("GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD", "4"),
            ("GIT_CACHE_COMPACTION_INLINE", "yes"),
        ]);

        let config = AppConfig::from_env().unwrap();
        assert_eq!(config.bind_addr.port(), 8080);
        assert_eq!(config.cache_root, PathBuf::from("/cache"));
        assert_eq!(
            config.allowed_upstream_hosts,
            vec!["github.com".to_string(), "gitlab.com".to_string()]
        );
        assert!(config.git_remote.enabled);
        assert!(!config.git_remote.commit_read_through);
        assert_eq!(config.git_remote.background_import_concurrency, 4);
        assert!(!config.git_remote.proxy_on_miss_by_default);
        assert_eq!(config.compaction.chain_depth_threshold, 4);
        assert!(config.compaction.inline);

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
            _ => panic!("expected s3 object store"),
        }
    }

    #[test]
    fn from_env_configures_gcs_object_store() {
        let _env = EnvGuard::new(&[
            ("GIT_CACHE_OBJECT_STORE_KIND", "gcs"),
            ("GIT_CACHE_GCS_BUCKET", "git-cache-bucket"),
            ("GIT_CACHE_GCS_PREFIX", "prod"),
            ("GIT_CACHE_GCS_ENDPOINT", "http://127.0.0.1:4443"),
        ]);

        let config = AppConfig::from_env().unwrap();
        match config.object_store {
            ObjectStoreConfig::Gcs {
                bucket,
                prefix,
                endpoint,
            } => {
                assert_eq!(bucket, "git-cache-bucket");
                assert_eq!(prefix, "prod");
                assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:4443"));
            }
            other => panic!("expected gcs object store, got {other:?}"),
        }
    }

    #[test]
    fn from_env_rejects_gcs_without_bucket() {
        let _env = EnvGuard::new(&[("GIT_CACHE_OBJECT_STORE_KIND", "gcs")]);
        assert!(AppConfig::from_env().is_err());
    }

    #[test]
    fn from_env_rejects_s3_without_bucket() {
        let _env = EnvGuard::new(&[("GIT_CACHE_OBJECT_STORE_KIND", "s3")]);
        assert!(AppConfig::from_env().is_err());
    }

    #[test]
    fn from_env_rejects_both_s3_and_gcs_buckets() {
        let _env = EnvGuard::new(&[
            ("GIT_CACHE_OBJECT_STORE_KIND", "s3"),
            ("GIT_CACHE_S3_BUCKET", "s3-bucket"),
            ("GIT_CACHE_GCS_BUCKET", "gcs-bucket"),
        ]);
        assert!(AppConfig::from_env().is_err());
    }

    #[test]
    fn from_env_rejects_s3_kind_with_gcs_bucket() {
        let _env = EnvGuard::new(&[
            ("GIT_CACHE_OBJECT_STORE_KIND", "s3"),
            ("GIT_CACHE_GCS_BUCKET", "gcs-bucket"),
        ]);
        assert!(AppConfig::from_env().is_err());
    }

    #[test]
    fn from_env_rejects_gcs_kind_with_s3_bucket() {
        let _env = EnvGuard::new(&[
            ("GIT_CACHE_OBJECT_STORE_KIND", "gcs"),
            ("GIT_CACHE_S3_BUCKET", "s3-bucket"),
        ]);
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
}
