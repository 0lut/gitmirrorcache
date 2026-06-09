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
    #[serde(default = "default_max_concurrent_generation_verifications")]
    pub max_concurrent_generation_verifications: usize,
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
            max_concurrent_generation_verifications: parse_env(
                "GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS",
                default_max_concurrent_generation_verifications(),
            )?,
        })
    }
}

fn object_store_from_env() -> crate::Result<ObjectStoreConfig> {
    match env::var("GIT_CACHE_OBJECT_STORE_KIND")
        .unwrap_or_else(|_| "local".into())
        .to_ascii_lowercase()
        .as_str()
    {
        "local" => Ok(ObjectStoreConfig::Local {
            root: PathBuf::from(
                env::var("GIT_CACHE_OBJECT_STORE_ROOT")
                    .unwrap_or_else(|_| "./tmp/object-store".into()),
            ),
        }),
        "s3" => {
            let bucket = env::var("GIT_CACHE_S3_BUCKET")
                .or_else(|_| env::var("GIT_CACHE_OBJECT_STORE_BUCKET"))
                .map_err(|_| {
                    crate::GitCacheError::Validation(
                        "GIT_CACHE_OBJECT_STORE_KIND=s3 requires GIT_CACHE_S3_BUCKET".into(),
                    )
                })?;
            if bucket.trim().is_empty() {
                return Err(crate::GitCacheError::Validation(
                    "GIT_CACHE_S3_BUCKET must not be empty".into(),
                ));
            }

            Ok(ObjectStoreConfig::S3 {
                bucket,
                prefix: env::var("GIT_CACHE_S3_PREFIX")
                    .or_else(|_| env::var("GIT_CACHE_OBJECT_STORE_PREFIX"))
                    .unwrap_or_else(|_| "repos".into()),
                endpoint: env::var("GIT_CACHE_S3_ENDPOINT")
                    .or_else(|_| env::var("GIT_CACHE_OBJECT_STORE_ENDPOINT"))
                    .ok()
                    .filter(|value| !value.trim().is_empty()),
            })
        }
        other => Err(crate::GitCacheError::Validation(format!(
            "unsupported GIT_CACHE_OBJECT_STORE_KIND `{other}`"
        ))),
    }
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

pub fn default_max_concurrent_generation_verifications() -> usize {
    1
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
        "GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS",
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
        assert_eq!(config.max_concurrent_generation_verifications, 1);
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
            ("GIT_CACHE_MAX_CONCURRENT_GENERATION_VERIFICATIONS", "3"),
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
        assert_eq!(config.max_concurrent_generation_verifications, 3);

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
}
