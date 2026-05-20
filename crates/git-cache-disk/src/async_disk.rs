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

    pub async fn reserve(&self, bytes: u64) -> Result<AsyncReservation> {
        let inner = self.inner.clone();
        let reservation = tokio::task::spawn_blocking(move || inner.reserve(bytes))
            .await
            .map_err(join_error)??;
        Ok(AsyncReservation::new(reservation))
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

/// Async-safe wrapper around [`Reservation`] that drops the inner reservation
/// inside `spawn_blocking` to avoid blocking I/O on the async runtime.
///
/// Callers should use [`AsyncReservation::release`] to perform an orderly cleanup.
/// If the reservation is dropped without calling `release()`, the `Drop` impl
/// logs a warning and falls through to the synchronous drop as a safety net.
pub struct AsyncReservation {
    inner: Option<Reservation>,
}

impl AsyncReservation {
    pub fn new(reservation: Reservation) -> Self {
        Self {
            inner: Some(reservation),
        }
    }

    pub fn temp_path(&self) -> PathBuf {
        self.inner
            .as_ref()
            .expect("AsyncReservation already released")
            .temp_path()
    }

    /// Consume this reservation, dropping the inner `Reservation` inside
    /// `spawn_blocking` so that the blocking filesystem cleanup does not
    /// run on the async executor.
    pub async fn release(mut self) -> Result<()> {
        if let Some(reservation) = self.inner.take() {
            tokio::task::spawn_blocking(move || drop(reservation))
                .await
                .map_err(join_error)?;
        }
        Ok(())
    }
}

impl Drop for AsyncReservation {
    fn drop(&mut self) {
        if self.inner.is_some() {
            tracing::warn!(
                "AsyncReservation dropped without calling release(); \
                 falling back to synchronous drop"
            );
            // The inner Option<Reservation> will be dropped here synchronously,
            // which triggers Reservation::drop and performs the cleanup.
        }
    }
}

fn join_error(err: tokio::task::JoinError) -> GitCacheError {
    GitCacheError::Io(std::io::Error::other(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiskManager;

    fn test_disk_manager() -> DiskManager {
        let root = std::env::temp_dir().join(format!(
            "git-cache-async-disk-test-{}",
            uuid::Uuid::now_v7()
        ));
        DiskManager::new(root, 1024 * 1024 * 1024, 0)
    }

    #[tokio::test]
    async fn release_cleans_up_temp_dirs_and_markers() {
        let dm = test_disk_manager();
        let async_dm = AsyncDiskManager::new(dm);
        let reservation = async_dm.reserve(1024).await.unwrap();
        let temp_path = reservation.temp_path();

        // temp dir should exist after reservation
        assert!(temp_path.exists());

        reservation.release().await.unwrap();

        // temp dir should be gone after release
        assert!(!temp_path.exists());
    }

    #[tokio::test]
    async fn drop_without_release_still_cleans_up() {
        let dm = test_disk_manager();
        let async_dm = AsyncDiskManager::new(dm);
        let reservation = async_dm.reserve(1024).await.unwrap();
        let temp_path = reservation.temp_path();

        assert!(temp_path.exists());

        // Drop without calling release
        drop(reservation);

        // Sync drop should still clean up
        assert!(!temp_path.exists());
    }
}
