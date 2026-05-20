use crate::{
    CleanupReport, DiskManager, DiskStatus, RepoIndex, RepoIndexEntry, RepoLock, Reservation,
};
use git_cache_core::{GitCacheError, Result};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct AsyncDiskManager {
    inner: DiskManager,
}

impl AsyncDiskManager {
    pub fn new(inner: DiskManager) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> &DiskManager {
        &self.inner
    }

    pub async fn reserve(&self, bytes: u64) -> Result<Reservation> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.reserve(bytes))
            .await
            .map_err(join_error)?
    }

    pub async fn status(&self) -> Result<DiskStatus> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.status())
            .await
            .map_err(join_error)?
    }

    pub async fn record_repo_access(&self, repo_path: PathBuf) -> Result<RepoIndexEntry> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.record_repo_access(repo_path))
            .await
            .map_err(join_error)?
    }

    pub async fn cleanup_stale_temps(&self, older_than: Duration) -> Result<CleanupReport> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.cleanup_stale_temps(older_than))
            .await
            .map_err(join_error)?
    }

    pub async fn lock_repo(&self, repo_path: PathBuf) -> Result<RepoLock> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.lock_repo(repo_path))
            .await
            .map_err(join_error)?
    }

    pub async fn repo_index(&self) -> Result<RepoIndex> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.repo_index())
            .await
            .map_err(join_error)?
    }
}

fn join_error(err: tokio::task::JoinError) -> GitCacheError {
    GitCacheError::Io(std::io::Error::new(std::io::ErrorKind::Other, err))
}
