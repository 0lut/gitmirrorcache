use async_trait::async_trait;
use chrono::Duration;
use git_cache_core::{GitCacheError, RepoKey, Result};
use git_cache_worker::{LeaseAcquire, RepoLease, RepoLeaseManager};
use sqlx::PgPool;
use tracing::warn;

pub struct PgRepoLeaseManager {
    pool: PgPool,
    holder: String,
    ttl: Duration,
}

impl PgRepoLeaseManager {
    pub fn new(pool: PgPool, holder: String, ttl: Duration) -> Self {
        Self { pool, holder, ttl }
    }

    /// Run migrations. Call once at startup.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(include_str!("../migrations/001_create_repo_leases.sql"))
            .execute(&self.pool)
            .await
            .map_err(|e| GitCacheError::Internal(format!("migration failed: {e}")))?;
        Ok(())
    }
}

#[async_trait]
impl RepoLeaseManager for PgRepoLeaseManager {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire> {
        let repo_key = repo.to_string();
        let ttl_interval = format!("{} seconds", self.ttl.num_seconds());

        let result = sqlx::query(
            "INSERT INTO repo_leases (repo_key, holder, acquired_at, expires_at)
             VALUES ($1, $2, now(), now() + $3::interval)
             ON CONFLICT (repo_key) DO UPDATE
               SET holder = EXCLUDED.holder,
                   acquired_at = EXCLUDED.acquired_at,
                   expires_at = EXCLUDED.expires_at
               WHERE repo_leases.expires_at < now()",
        )
        .bind(&repo_key)
        .bind(&self.holder)
        .bind(&ttl_interval)
        .execute(&self.pool)
        .await
        .map_err(|e| GitCacheError::Internal(format!("lease acquire failed: {e}")))?;

        if result.rows_affected() == 1 {
            Ok(LeaseAcquire::Acquired(Box::new(PgRepoLease {
                pool: self.pool.clone(),
                repo_key,
                holder: self.holder.clone(),
                ttl_interval,
                released: false,
            })))
        } else {
            Ok(LeaseAcquire::Busy)
        }
    }
}

pub struct PgRepoLease {
    pool: PgPool,
    repo_key: String,
    holder: String,
    ttl_interval: String,
    released: bool,
}

impl PgRepoLease {
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
        let pool = self.pool.clone();
        let repo_key = self.repo_key.clone();
        let holder = self.holder.clone();
        tokio::spawn(async move {
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
                let mgr = PgRepoLeaseManager::new(
                    pool,
                    format!("holder-{i}"),
                    Duration::minutes(5),
                );
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
}
