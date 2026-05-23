use git_cache_core::{AppConfig, GitCacheError, ObjectStoreConfig, Result as CoreResult};
use git_cache_disk::{AsyncDiskManager, DiskManager};
use git_cache_git::Git;
use git_cache_objectstore::{LocalObjectStore, ObjectStore};
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
            ObjectStoreConfig::S3 { .. } => {
                return Err(GitCacheError::NotImplemented(
                    "S3 object store wiring is provided by the objectstore crate and not enabled in the API yet"
                        .into(),
                ))
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
