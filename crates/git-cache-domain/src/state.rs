#[cfg(feature = "s3")]
use aws_config::BehaviorVersion;
#[cfg(feature = "s3")]
use aws_credential_types::Credentials;
#[cfg(feature = "s3")]
use aws_sdk_s3::config::RequestChecksumCalculation;
#[cfg(feature = "s3")]
use aws_sdk_s3::Client;
#[cfg(feature = "s3")]
use aws_types::region::Region;
use git_cache_core::{AppConfig, GitCacheError, ObjectStoreConfig, Result as CoreResult};
use git_cache_disk::{AsyncDiskManager, DiskManager};
use git_cache_git::Git;
#[cfg(feature = "s3")]
use git_cache_objectstore::S3ObjectStore;
use git_cache_objectstore::{LocalObjectStore, ObjectStore};
use std::collections::HashSet;
#[cfg(feature = "s3")]
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::info;

/// Shared with `scripts/aws/deploy-ecs-ec2-ebs.sh` via the sibling
/// `object-store-schema-suffix` file so the deploy-time S3 prefix and the
/// runtime schema suffix cannot drift.
const OBJECT_STORE_SCHEMA_SUFFIX: &str = include_str!("../object-store-schema-suffix").trim_ascii();

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub store: Arc<dyn ObjectStore>,
    pub git: Git,
    pub disk: AsyncDiskManager,
    /// Repo dirs with a queued or running background serving-maintenance
    /// task (repack + commit-graph), to dedupe bursts of hydrating requests.
    pub serving_maintenance_inflight: Arc<Mutex<HashSet<PathBuf>>>,
}

impl AppState {
    pub fn try_new(config: AppConfig) -> CoreResult<Self> {
        let store: Arc<dyn ObjectStore> = match &config.object_store {
            ObjectStoreConfig::Local { root } => {
                Arc::new(LocalObjectStore::new(v2_local_store_root(root)))
            }
            ObjectStoreConfig::S3 {
                bucket,
                prefix,
                endpoint,
            } => {
                #[cfg(feature = "s3")]
                {
                    Arc::new(s3_store(
                        bucket,
                        &v2_s3_prefix(prefix),
                        endpoint.as_deref(),
                    )?)
                }
                #[cfg(not(feature = "s3"))]
                {
                    let _ = (bucket, prefix, endpoint);
                    return Err(GitCacheError::NotImplemented(
                        "S3 object store wiring requires the s3 feature".into(),
                    ));
                }
            }
        };

        Self::with_store(config, store)
    }

    pub async fn try_new_async(config: AppConfig) -> CoreResult<Self> {
        let store: Arc<dyn ObjectStore> = match &config.object_store {
            ObjectStoreConfig::Local { root } => {
                Arc::new(LocalObjectStore::new(v2_local_store_root(root)))
            }
            ObjectStoreConfig::S3 {
                bucket,
                prefix,
                endpoint,
            } => {
                #[cfg(feature = "s3")]
                {
                    Arc::new(
                        s3_store_async(bucket, &v2_s3_prefix(prefix), endpoint.as_deref()).await?,
                    )
                }
                #[cfg(not(feature = "s3"))]
                {
                    let _ = (bucket, prefix, endpoint);
                    return Err(GitCacheError::NotImplemented(
                        "S3 object store wiring requires the s3 feature".into(),
                    ));
                }
            }
        };

        Self::with_store(config, store)
    }

    fn with_store(config: AppConfig, store: Arc<dyn ObjectStore>) -> CoreResult<Self> {
        let git = Git::with_concurrency_limit(
            config.git_binary.clone(),
            Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes)
        .with_gitoxide(config.use_gitoxide);
        let git = with_optional_upstream_credentials(git, &config);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        // On process start, in-memory reservations are empty; anything left on
        // disk is scratch from a previous process and must not block new work.
        let startup_cleanup = disk.cleanup_stale_temps(Duration::ZERO)?;
        if startup_cleanup.removed_temp_dirs > 0
            || startup_cleanup.removed_reservation_markers > 0
            || startup_cleanup.freed_bytes > 0
        {
            info!(
                removed_temp_dirs = startup_cleanup.removed_temp_dirs,
                removed_reservation_markers = startup_cleanup.removed_reservation_markers,
                freed_bytes = startup_cleanup.freed_bytes,
                "cleaned stale disk reservations on startup"
            );
        }
        Ok(Self {
            config,
            store,
            git,
            disk: AsyncDiskManager::new(disk),
            serving_maintenance_inflight: Arc::new(Mutex::new(HashSet::new())),
        })
    }
}

fn v2_local_store_root(root: &Path) -> PathBuf {
    let Some(file_name) = root.file_name().and_then(|name| name.to_str()) else {
        return root.join(OBJECT_STORE_SCHEMA_SUFFIX);
    };
    if is_v2_component(file_name) {
        return root.to_path_buf();
    }
    root.with_file_name(format!("{file_name}-{OBJECT_STORE_SCHEMA_SUFFIX}"))
}

#[cfg(any(feature = "s3", test))]
fn v2_s3_prefix(prefix: &str) -> String {
    let normalized = prefix.trim_matches('/');
    if normalized.is_empty() {
        return OBJECT_STORE_SCHEMA_SUFFIX.to_string();
    }

    if let Some((parent, component)) = normalized.rsplit_once('/') {
        if is_v2_component(component) {
            normalized.to_string()
        } else {
            format!("{parent}/{component}-{OBJECT_STORE_SCHEMA_SUFFIX}")
        }
    } else if is_v2_component(normalized) {
        normalized.to_string()
    } else {
        format!("{normalized}-{OBJECT_STORE_SCHEMA_SUFFIX}")
    }
}

fn is_v2_component(component: &str) -> bool {
    component == OBJECT_STORE_SCHEMA_SUFFIX
        || component.ends_with(&format!("-{OBJECT_STORE_SCHEMA_SUFFIX}"))
}

#[cfg(feature = "s3")]
fn s3_store(bucket: &str, prefix: &str, endpoint: Option<&str>) -> CoreResult<S3ObjectStore> {
    let access_key = env::var("GIT_CACHE_S3_ACCESS_KEY")
        .or_else(|_| env::var("AWS_ACCESS_KEY_ID"))
        .map_err(|_| {
            GitCacheError::Validation(
                "S3 object store requires GIT_CACHE_S3_ACCESS_KEY or AWS_ACCESS_KEY_ID".into(),
            )
        })?;
    let secret_key = env::var("GIT_CACHE_S3_SECRET_KEY")
        .or_else(|_| env::var("AWS_SECRET_ACCESS_KEY"))
        .map_err(|_| {
            GitCacheError::Validation(
                "S3 object store requires GIT_CACHE_S3_SECRET_KEY or AWS_SECRET_ACCESS_KEY".into(),
            )
        })?;
    let session_token = env::var("GIT_CACHE_S3_SESSION_TOKEN")
        .or_else(|_| env::var("AWS_SESSION_TOKEN"))
        .ok();
    let region = env::var("GIT_CACHE_S3_REGION")
        .or_else(|_| env::var("AWS_REGION"))
        .unwrap_or_else(|_| "us-east-1".into());

    let mut config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(region))
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .credentials_provider(Credentials::new(
            access_key,
            secret_key,
            session_token,
            None,
            "git-cache-s3-env",
        ));

    if let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty()) {
        config = config.endpoint_url(endpoint).force_path_style(true);
    }

    S3ObjectStore::new(
        Client::from_conf(config.build()),
        bucket.to_string(),
        prefix.to_string(),
    )
}

#[cfg(feature = "s3")]
async fn s3_store_async(
    bucket: &str,
    prefix: &str,
    endpoint: Option<&str>,
) -> CoreResult<S3ObjectStore> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());

    if let Some(region) = s3_region_from_env() {
        loader = loader.region(Region::new(region));
    }

    if let Some(credentials) = explicit_s3_credentials()? {
        loader = loader.credentials_provider(credentials);
    }

    let sdk_config = loader.load().await;
    let mut config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
        .credentials_provider(
            sdk_config
                .credentials_provider()
                .ok_or_else(|| {
                    GitCacheError::Validation(
                        "S3 object store requires AWS credentials from env, profile, or role"
                            .into(),
                    )
                })?
                .clone(),
        );

    if let Some(region) = sdk_config.region().cloned() {
        config = config.region(region);
    }

    if let Some(endpoint) = endpoint.filter(|value| !value.trim().is_empty()) {
        config = config.endpoint_url(endpoint).force_path_style(true);
    }

    S3ObjectStore::new(
        Client::from_conf(config.build()),
        bucket.to_string(),
        prefix.to_string(),
    )
}

#[cfg(feature = "s3")]
fn explicit_s3_credentials() -> CoreResult<Option<Credentials>> {
    let access_key = env::var("GIT_CACHE_S3_ACCESS_KEY").ok();
    let secret_key = env::var("GIT_CACHE_S3_SECRET_KEY").ok();
    if access_key.is_none() && secret_key.is_none() {
        return Ok(None);
    }

    let access_key = access_key.ok_or_else(|| {
        GitCacheError::Validation("GIT_CACHE_S3_SECRET_KEY requires GIT_CACHE_S3_ACCESS_KEY".into())
    })?;
    let secret_key = secret_key.ok_or_else(|| {
        GitCacheError::Validation("GIT_CACHE_S3_ACCESS_KEY requires GIT_CACHE_S3_SECRET_KEY".into())
    })?;
    let session_token = env::var("GIT_CACHE_S3_SESSION_TOKEN")
        .or_else(|_| env::var("AWS_SESSION_TOKEN"))
        .ok();

    Ok(Some(Credentials::new(
        access_key,
        secret_key,
        session_token,
        None,
        "git-cache-s3-env",
    )))
}

#[cfg(feature = "s3")]
fn s3_region_from_env() -> Option<String> {
    env::var("GIT_CACHE_S3_REGION")
        .or_else(|_| env::var("AWS_REGION"))
        .or_else(|_| env::var("AWS_DEFAULT_REGION"))
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub fn with_optional_upstream_credentials(git: Git, config: &AppConfig) -> Git {
    let Some(token_env) = &config.upstream_auth_token_env else {
        return git;
    };
    let Ok(token) = std::env::var(token_env) else {
        return git;
    };
    if token.trim().is_empty() {
        return git;
    }

    let host = config
        .allowed_upstream_hosts
        .first()
        .map(String::as_str)
        .unwrap_or("github.com");

    git.with_env("GIT_CONFIG_COUNT", "1")
        .with_env(
            "GIT_CONFIG_KEY_0",
            format!("http.https://{host}/.extraHeader"),
        )
        .with_env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
}

#[cfg(test)]
mod tests {
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
            max_concurrent_git_processes: 1,
            async_materialize_concurrency: 2,
            use_gitoxide: true,
        }
    }
}
