use serde::{Deserialize, Serialize};
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub bind_addr: SocketAddr,
    pub public_base_url: String,
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
    pub session_ttl_seconds: u64,
    #[serde(default)]
    pub upstream_auth_token_env: Option<String>,
    #[serde(default = "default_rate_limit_per_minute")]
    pub rate_limit_per_minute: u32,
    #[serde(default = "default_allowed_upstream_hosts")]
    pub allowed_upstream_hosts: Vec<String>,
    pub disk: DiskConfig,
    #[serde(default)]
    pub git_remote: GitRemoteConfig,
}

impl AppConfig {
    pub fn from_path(path: impl AsRef<Path>) -> crate::Result<Self> {
        let raw = fs::read_to_string(path)?;
        toml::from_str(&raw)
            .map_err(|err| crate::GitCacheError::Validation(format!("invalid config file: {err}")))
    }

    pub fn from_env() -> crate::Result<Self> {
        if let Ok(path) = std::env::var("GIT_CACHE_CONFIG") {
            return Self::from_path(path);
        }

        let bind_addr = std::env::var("GIT_CACHE_BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()
            .map_err(|err| crate::GitCacheError::Validation(format!("invalid bind addr: {err}")))?;

        let public_base_url = std::env::var("GIT_CACHE_PUBLIC_BASE_URL")
            .unwrap_or_else(|_| format!("http://{bind_addr}"));

        let cache_root =
            PathBuf::from(std::env::var("GIT_CACHE_ROOT").unwrap_or_else(|_| "./cache".into()));
        let object_root = PathBuf::from(
            std::env::var("GIT_CACHE_OBJECT_STORE_ROOT")
                .unwrap_or_else(|_| "./tmp/object-store".into()),
        );
        let upstream_root = std::env::var("GIT_CACHE_UPSTREAM_ROOT")
            .ok()
            .map(PathBuf::from);

        Ok(Self {
            bind_addr,
            public_base_url,
            cache_root,
            upstream_root,
            git_binary: PathBuf::from(
                std::env::var("GIT_CACHE_GIT_BINARY").unwrap_or_else(|_| "git".into()),
            ),
            git_timeout_seconds: parse_env_u64(
                "GIT_CACHE_GIT_TIMEOUT_SECONDS",
                default_git_timeout_seconds(),
            )?,
            max_git_output_bytes: parse_env_usize(
                "GIT_CACHE_MAX_GIT_OUTPUT_BYTES",
                default_max_git_output_bytes(),
            )?,
            object_store: ObjectStoreConfig::Local { root: object_root },
            session_ttl_seconds: parse_env_u64("GIT_CACHE_SESSION_TTL_SECONDS", 3600)?,
            upstream_auth_token_env: std::env::var("GIT_CACHE_UPSTREAM_AUTH_TOKEN_ENV").ok(),
            rate_limit_per_minute: parse_env_u32(
                "GIT_CACHE_RATE_LIMIT_PER_MINUTE",
                default_rate_limit_per_minute(),
            )?,
            allowed_upstream_hosts: default_allowed_upstream_hosts(),
            disk: DiskConfig {
                quota_bytes: parse_env_u64("GIT_CACHE_DISK_QUOTA_BYTES", 10 * 1024 * 1024 * 1024)?,
                min_free_bytes: parse_env_u64("GIT_CACHE_DISK_MIN_FREE_BYTES", 1024 * 1024 * 1024)?,
            },
            git_remote: GitRemoteConfig::default(),
        })
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
pub struct GitRemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_branch_ref_check")]
    pub branch_ref_check: BranchRefCheck,
    #[serde(default = "default_true")]
    pub commit_read_through: bool,
}

impl Default for GitRemoteConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            branch_ref_check: BranchRefCheck::Always,
            commit_read_through: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchRefCheck {
    Always,
}

fn default_branch_ref_check() -> BranchRefCheck {
    BranchRefCheck::Always
}

fn default_true() -> bool {
    true
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

fn parse_env_u64(name: &str, default: u64) -> crate::Result<u64> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|err| {
            crate::GitCacheError::Validation(format!("invalid {name} value `{value}`: {err}"))
        }),
        Err(_) => Ok(default),
    }
}

fn parse_env_u32(name: &str, default: u32) -> crate::Result<u32> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|err| {
            crate::GitCacheError::Validation(format!("invalid {name} value `{value}`: {err}"))
        }),
        Err(_) => Ok(default),
    }
}

fn parse_env_usize(name: &str, default: usize) -> crate::Result<usize> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|err| {
            crate::GitCacheError::Validation(format!("invalid {name} value `{value}`: {err}"))
        }),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn from_path_parses_valid_toml() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
bind_addr = "127.0.0.1:9090"
public_base_url = "http://localhost:9090"
cache_root = "/tmp/cache"
git_timeout_seconds = 60
session_ttl_seconds = 1800
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
        assert_eq!(config.session_ttl_seconds, 1800);
        assert_eq!(config.rate_limit_per_minute, 500);
    }

    #[test]
    fn from_path_uses_defaults_for_omitted_fields() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        write!(
            tmp,
            r#"
bind_addr = "0.0.0.0:8080"
public_base_url = "http://example.com"
cache_root = "/cache"
session_ttl_seconds = 3600

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
    }

    #[test]
    fn git_remote_config_default_values() {
        let config = GitRemoteConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.branch_ref_check, BranchRefCheck::Always);
        assert!(config.commit_read_through);
    }

    #[test]
    fn git_remote_config_serde_round_trip() {
        let config = GitRemoteConfig {
            enabled: true,
            branch_ref_check: BranchRefCheck::Always,
            commit_read_through: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: GitRemoteConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
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
