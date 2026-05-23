#[cfg(feature = "s3")]
use aws_credential_types::Credentials;
#[cfg(feature = "s3")]
use aws_sdk_s3::config::{BehaviorVersion, RequestChecksumCalculation};
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
#[cfg(feature = "s3")]
use std::env;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub store: Arc<dyn ObjectStore>,
    pub git: Git,
    pub disk: AsyncDiskManager,
}

impl AppState {
    pub fn try_new(config: AppConfig) -> CoreResult<Self> {
        let store: Arc<dyn ObjectStore> = match &config.object_store {
            ObjectStoreConfig::Local { root } => Arc::new(LocalObjectStore::new(root)),
            ObjectStoreConfig::S3 {
                bucket,
                prefix,
                endpoint,
            } => {
                #[cfg(feature = "s3")]
                {
                    Arc::new(s3_store(bucket, prefix, endpoint.as_deref())?)
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

        let git = Git::with_concurrency_limit(
            config.git_binary.clone(),
            Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes);
        let git = with_optional_upstream_credentials(git, &config);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );

        Ok(Self {
            config,
            store,
            git,
            disk: AsyncDiskManager::new(disk),
        })
    }
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
