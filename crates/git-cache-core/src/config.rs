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
    #[serde(default)]
    pub shutdown: ShutdownConfig,
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
                access_flush_interval_secs: parse_env(
                    "GIT_CACHE_DISK_ACCESS_FLUSH_SECS",
                    default_disk_access_flush_secs(),
                )?,
            },
            git_remote: GitRemoteConfig {
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
                proxy_tee_import: parse_bool_env("GIT_CACHE_GIT_REMOTE_PROXY_TEE_IMPORT", true)?,
            },
            compaction: CompactionConfig {
                chain_depth_threshold: parse_env(
                    "GIT_CACHE_COMPACTION_CHAIN_DEPTH_THRESHOLD",
                    default_compaction_threshold(),
                )?,
                inline: parse_bool_env("GIT_CACHE_COMPACTION_INLINE", false)?,
                retention_secs: parse_env(
                    "GIT_CACHE_COMPACTION_RETENTION_SECS",
                    default_compaction_retention_secs(),
                )?,
            },
            shutdown: ShutdownConfig {
                readiness_delay_seconds: parse_env(
                    "GIT_CACHE_SHUTDOWN_READINESS_DELAY_SECONDS",
                    default_shutdown_readiness_delay_seconds(),
                )?,
                drain_timeout_seconds: parse_env(
                    "GIT_CACHE_SHUTDOWN_DRAIN_TIMEOUT_SECONDS",
                    default_shutdown_drain_timeout_seconds(),
                )?,
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
            let bucket = env::var("GIT_CACHE_S3_BUCKET").map_err(|_| {
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
                prefix: env::var("GIT_CACHE_S3_PREFIX").unwrap_or_else(|_| "repos".into()),
                endpoint: env::var("GIT_CACHE_S3_ENDPOINT")
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
    /// How often buffered repo-access timestamps are flushed from memory to
    /// the on-disk repo index.
    #[serde(default = "default_disk_access_flush_secs")]
    pub access_flush_interval_secs: u64,
}

fn default_disk_access_flush_secs() -> u64 {
    60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_compaction_threshold")]
    pub chain_depth_threshold: u32,
    #[serde(default)]
    pub inline: bool,
    /// How long a superseded generation is kept before the retention sweep
    /// may delete it, measured from its successor's `created_at`.
    #[serde(default = "default_compaction_retention_secs")]
    pub retention_secs: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            chain_depth_threshold: default_compaction_threshold(),
            inline: false,
            retention_secs: default_compaction_retention_secs(),
        }
    }
}

/// Graceful shutdown behavior for the API server. On SIGTERM/SIGINT the
/// server first fails readiness (`/healthz` returns 503) for
/// `readiness_delay_seconds` so load balancers stop routing new traffic,
/// then stops accepting connections and drains in-flight requests for up to
/// `drain_timeout_seconds` before exiting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownConfig {
    #[serde(default = "default_shutdown_readiness_delay_seconds")]
    pub readiness_delay_seconds: u64,
    #[serde(default = "default_shutdown_drain_timeout_seconds")]
    pub drain_timeout_seconds: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            readiness_delay_seconds: default_shutdown_readiness_delay_seconds(),
            drain_timeout_seconds: default_shutdown_drain_timeout_seconds(),
        }
    }
}

fn default_compaction_threshold() -> u32 {
    10
}

fn default_compaction_retention_secs() -> u64 {
    24 * 60 * 60
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRemoteConfig {
    #[serde(default = "default_true")]
    pub commit_read_through: bool,
    #[serde(default = "default_background_import_concurrency")]
    pub background_import_concurrency: usize,
    #[serde(default = "default_true")]
    pub proxy_on_miss_by_default: bool,
    /// When proxying a cold miss upstream, tee the proxied upload-pack
    /// response into the local cache instead of re-fetching upstream in the
    /// background warm.
    #[serde(default = "default_true")]
    pub proxy_tee_import: bool,
}

impl Default for GitRemoteConfig {
    fn default() -> Self {
        Self {
            commit_read_through: true,
            background_import_concurrency: default_background_import_concurrency(),
            proxy_on_miss_by_default: true,
            proxy_tee_import: true,
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
    8
}

pub fn default_use_gitoxide() -> bool {
    true
}

fn default_shutdown_readiness_delay_seconds() -> u64 {
    5
}

fn default_shutdown_drain_timeout_seconds() -> u64 {
    60
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
mod tests;
