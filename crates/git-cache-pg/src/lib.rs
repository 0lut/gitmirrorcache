use async_trait::async_trait;
use chrono::Duration;
use git_cache_core::{GitCacheError, RepoKey, Result};
use git_cache_worker::{LeaseAcquire, RepoLease, RepoLeaseManager};
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

/// Default lease TTL. Kept short so that a crashed worker's leases expire
/// quickly. The background heartbeat renews the lease while the holder is
/// alive.
const DEFAULT_LEASE_TTL: Duration = Duration::seconds(30);

/// Heartbeat renewal interval — renew at TTL/3 to give ~2 retries before
/// expiry.
fn heartbeat_interval(ttl: &Duration) -> std::time::Duration {
    let secs = ttl.num_seconds().max(3) / 3;
    std::time::Duration::from_secs(secs as u64)
}

pub struct PgRepoLeaseManager {
    pool: PgPool,
    holder: String,
    ttl: Duration,
}

impl PgRepoLeaseManager {
    pub fn new(pool: PgPool, holder: String, ttl: Duration) -> Self {
        Self { pool, holder, ttl }
    }

    /// Create with the default 30-second TTL.
    pub fn with_default_ttl(pool: PgPool, holder: String) -> Self {
        Self::new(pool, holder, DEFAULT_LEASE_TTL)
    }

    /// Run migrations. Call once at startup.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::raw_sql(include_str!("../migrations/001_create_repo_leases.sql"))
            .execute(&self.pool)
            .await
            .map_err(|e| GitCacheError::Internal(format!("migration failed: {e}")))?;
        Ok(())
    }

    /// Release all leases held by this worker. Call during graceful shutdown.
    pub async fn release_all(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM repo_leases WHERE holder = $1")
            .bind(&self.holder)
            .execute(&self.pool)
            .await
            .map_err(|e| GitCacheError::Internal(format!("release_all failed: {e}")))?;
        Ok(result.rows_affected())
    }

    /// Reap expired leases from any holder. Call periodically for non-graceful
    /// termination cleanup (e.g. workers that crashed without releasing).
    pub async fn reap_expired(&self) -> Result<u64> {
        let result = sqlx::query("DELETE FROM repo_leases WHERE expires_at < now()")
            .execute(&self.pool)
            .await
            .map_err(|e| GitCacheError::Internal(format!("reap_expired failed: {e}")))?;
        Ok(result.rows_affected())
    }

    /// Spawn a background loop that reaps expired leases every `interval`.
    /// Returns a handle that can be aborted on shutdown.
    pub fn spawn_reaper(&self, interval: std::time::Duration) -> JoinHandle<()> {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let result = sqlx::query("DELETE FROM repo_leases WHERE expires_at < now()")
                    .execute(&pool)
                    .await;
                if let Err(e) = result {
                    warn!(%e, "lease reaper failed");
                }
            }
        })
    }
}

#[async_trait]
impl RepoLeaseManager for PgRepoLeaseManager {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire> {
        let repo_key = repo.to_string();
        let ttl_interval = format!("{} seconds", self.ttl.num_seconds());

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| GitCacheError::Internal(format!("lease tx begin failed: {e}")))?;

        // Lock the row if it exists (SELECT ... FOR UPDATE)
        let existing: Option<(String, bool)> = sqlx::query_as(
            "SELECT holder, expires_at < now() AS expired
             FROM repo_leases
             WHERE repo_key = $1
             FOR UPDATE",
        )
        .bind(&repo_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| GitCacheError::Internal(format!("lease select failed: {e}")))?;

        let acquired = match existing {
            None => {
                // No row — insert new lease
                sqlx::query(
                    "INSERT INTO repo_leases (repo_key, holder, acquired_at, expires_at)
                     VALUES ($1, $2, now(), now() + $3::interval)",
                )
                .bind(&repo_key)
                .bind(&self.holder)
                .bind(&ttl_interval)
                .execute(&mut *tx)
                .await
                .map_err(|e| GitCacheError::Internal(format!("lease insert failed: {e}")))?;
                true
            }
            Some((ref holder, expired)) => {
                if holder == &self.holder || expired {
                    // Same holder re-acquiring or expired lease — take over
                    sqlx::query(
                        "UPDATE repo_leases
                         SET holder = $1, acquired_at = now(), expires_at = now() + $2::interval
                         WHERE repo_key = $3",
                    )
                    .bind(&self.holder)
                    .bind(&ttl_interval)
                    .bind(&repo_key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| GitCacheError::Internal(format!("lease update failed: {e}")))?;
                    true
                } else {
                    // Held by another worker and not expired
                    false
                }
            }
        };

        tx.commit()
            .await
            .map_err(|e| GitCacheError::Internal(format!("lease tx commit failed: {e}")))?;

        if acquired {
            // Spawn heartbeat renewal task
            let (stop_tx, stop_rx) = watch::channel(false);
            let heartbeat = spawn_heartbeat(
                self.pool.clone(),
                repo_key.clone(),
                self.holder.clone(),
                ttl_interval.clone(),
                self.ttl,
                stop_rx,
            );

            Ok(LeaseAcquire::Acquired(Box::new(PgRepoLease {
                pool: self.pool.clone(),
                repo_key,
                holder: self.holder.clone(),
                ttl_interval,
                released: false,
                _stop_tx: stop_tx,
                _heartbeat: heartbeat,
            })))
        } else {
            Ok(LeaseAcquire::Busy)
        }
    }
}

fn spawn_heartbeat(
    pool: PgPool,
    repo_key: String,
    holder: String,
    ttl_interval: String,
    ttl: Duration,
    mut stop_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let interval = heartbeat_interval(&ttl);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the first immediate tick
        tick.tick().await;

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let result = sqlx::query(
                        "UPDATE repo_leases SET expires_at = now() + $1::interval
                         WHERE repo_key = $2 AND holder = $3",
                    )
                    .bind(&ttl_interval)
                    .bind(&repo_key)
                    .bind(&holder)
                    .execute(&pool)
                    .await;

                    if let Err(e) = result {
                        warn!(%e, %repo_key, "heartbeat renewal failed");
                    }
                }
                _ = stop_rx.changed() => {
                    break;
                }
            }
        }
    })
}

pub struct PgRepoLease {
    pool: PgPool,
    repo_key: String,
    holder: String,
    ttl_interval: String,
    released: bool,
    _stop_tx: watch::Sender<bool>,
    _heartbeat: JoinHandle<()>,
}

impl PgRepoLease {
    /// Manually renew the lease (extends expiry by TTL).
    pub async fn renew(&self) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE repo_leases SET expires_at = now() + $1::interval
             WHERE repo_key = $2 AND holder = $3",
        )
        .bind(&self.ttl_interval)
        .bind(&self.repo_key)
        .bind(&self.holder)
        .execute(&self.pool)
        .await
        .map_err(|e| GitCacheError::Internal(format!("lease renew failed: {e}")))?;
        Ok(result.rows_affected() == 1)
    }
}

#[async_trait]
impl RepoLease for PgRepoLease {
    async fn release(mut self: Box<Self>) -> Result<()> {
        self.released = true;
        // Signal heartbeat to stop
        let _ = self._stop_tx.send(true);
        self._heartbeat.abort();
        sqlx::query("DELETE FROM repo_leases WHERE repo_key = $1 AND holder = $2")
            .bind(&self.repo_key)
            .bind(&self.holder)
            .execute(&self.pool)
            .await
            .map_err(|e| GitCacheError::Internal(format!("lease release failed: {e}")))?;
        Ok(())
    }
}

impl Drop for PgRepoLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Stop heartbeat
        let _ = self._stop_tx.send(true);
        self._heartbeat.abort();

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = self.pool.clone();
        let repo_key = self.repo_key.clone();
        let holder = self.holder.clone();
        handle.spawn(async move {
            if let Err(e) =
                sqlx::query("DELETE FROM repo_leases WHERE repo_key = $1 AND holder = $2")
                    .bind(&repo_key)
                    .bind(&holder)
                    .execute(&pool)
                    .await
            {
                warn!(%e, %repo_key, "best-effort lease cleanup failed");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    async fn test_pool() -> Option<PgPool> {
        let url = match std::env::var("DATABASE_URL") {
            Ok(u) => u,
            Err(_) => return None,
        };
        let pool = PgPool::connect(&url).await.ok()?;
        Some(pool)
    }

    async fn setup() -> Option<(PgPool, PgRepoLeaseManager)> {
        let pool = test_pool().await?;
        let mgr = PgRepoLeaseManager::new(pool.clone(), "test-holder".into(), Duration::minutes(5));
        mgr.migrate().await.unwrap();
        // Clean up any stale test data
        sqlx::query("DELETE FROM repo_leases WHERE repo_key LIKE 'test/%'")
            .execute(&pool)
            .await
            .unwrap();
        Some((pool, mgr))
    }

    #[tokio::test]
    async fn acquire_returns_acquired_for_new_repo() {
        let Some((_pool, mgr)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/acquire-new").unwrap();
        let result = mgr.acquire(&repo).await.unwrap();
        assert!(matches!(result, LeaseAcquire::Acquired(_)));
    }

    #[tokio::test]
    async fn acquire_returns_busy_for_held_repo() {
        let Some((_pool, mgr)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/acquire-busy").unwrap();
        let lease = mgr.acquire(&repo).await.unwrap();
        assert!(matches!(lease, LeaseAcquire::Acquired(_)));

        let second = mgr.acquire(&repo).await.unwrap();
        assert!(matches!(second, LeaseAcquire::Busy));

        // Clean up
        if let LeaseAcquire::Acquired(l) = lease {
            l.release().await.unwrap();
        }
    }

    #[tokio::test]
    async fn acquire_succeeds_for_expired_lease() {
        let Some((pool, _)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/acquire-expired").unwrap();
        // Create a manager with 0-second TTL so lease expires immediately
        let mgr = PgRepoLeaseManager::new(pool.clone(), "holder-a".into(), Duration::seconds(0));
        mgr.migrate().await.unwrap();

        let lease = mgr.acquire(&repo).await.unwrap();
        assert!(matches!(lease, LeaseAcquire::Acquired(_)));
        // Don't release — let it expire

        // Small delay to ensure expiry
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        let mgr_b = PgRepoLeaseManager::new(pool, "holder-b".into(), Duration::minutes(5));
        let result = mgr_b.acquire(&repo).await.unwrap();
        assert!(matches!(result, LeaseAcquire::Acquired(_)));
    }

    #[tokio::test]
    async fn release_allows_reacquire() {
        let Some((_pool, mgr)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/release-reacquire").unwrap();
        let lease = mgr.acquire(&repo).await.unwrap();
        if let LeaseAcquire::Acquired(l) = lease {
            l.release().await.unwrap();
        }
        let result = mgr.acquire(&repo).await.unwrap();
        assert!(matches!(result, LeaseAcquire::Acquired(_)));
    }

    #[tokio::test]
    async fn concurrent_acquires_only_one_wins() {
        let Some((pool, _)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/concurrent").unwrap();
        let n = 10;
        let mut handles = Vec::new();

        for i in 0..n {
            let pool = pool.clone();
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                let mgr =
                    PgRepoLeaseManager::new(pool, format!("holder-{i}"), Duration::minutes(5));
                mgr.acquire(&repo).await
            }));
        }

        let mut acquired = 0;
        for h in handles {
            if let Ok(Ok(LeaseAcquire::Acquired(_))) = h.await {
                acquired += 1;
            }
        }
        assert_eq!(acquired, 1, "exactly one concurrent acquire should win");
    }

    #[tokio::test]
    async fn release_all_clears_holder_leases() {
        let Some((pool, mgr)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo_a = RepoKey::parse("test/release-all-a").unwrap();
        let repo_b = RepoKey::parse("test/release-all-b").unwrap();

        let _lease_a = mgr.acquire(&repo_a).await.unwrap();
        let _lease_b = mgr.acquire(&repo_b).await.unwrap();

        let released = mgr.release_all().await.unwrap();
        assert_eq!(released, 2);

        // Another holder can now acquire
        let mgr_b = PgRepoLeaseManager::new(pool, "other-holder".into(), Duration::minutes(5));
        let result = mgr_b.acquire(&repo_a).await.unwrap();
        assert!(matches!(result, LeaseAcquire::Acquired(_)));
    }

    #[tokio::test]
    async fn reap_expired_removes_stale_leases() {
        let Some((pool, _)) = setup().await else {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        };
        let repo = RepoKey::parse("test/reap-expired").unwrap();
        let mgr =
            PgRepoLeaseManager::new(pool.clone(), "crash-holder".into(), Duration::seconds(0));
        mgr.migrate().await.unwrap();

        let _lease = mgr.acquire(&repo).await.unwrap();
        tokio::time::sleep(StdDuration::from_millis(50)).await;

        let reaper = PgRepoLeaseManager::new(pool.clone(), "reaper".into(), Duration::minutes(5));
        let reaped = reaper.reap_expired().await.unwrap();
        assert!(reaped >= 1);

        // Verify the repo can now be acquired
        let result = reaper.acquire(&repo).await.unwrap();
        assert!(matches!(result, LeaseAcquire::Acquired(_)));
    }
}
