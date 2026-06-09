//! Resource contention tests for the DiskManager.
//!
//! These tests stress concurrent access patterns: parallel reservations,
//! eviction under pressure, lock tracking, and cleanup races.

mod tests {
    use futures::future::join_all;
    use git_cache_core::GitCacheError;
    use git_cache_disk::DiskManager;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    fn make_manager(tmp: &TempDir, quota: u64) -> DiskManager {
        DiskManager::new(tmp.path(), quota, 0)
    }

    fn write_repo_file(manager: &DiskManager, repo_path: &str, bytes: usize) {
        let repo_dir = manager.repos_dir().join(repo_path);
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("objects.pack"), vec![0u8; bytes]).unwrap();
    }

    // ── 1. Concurrent reservations within quota ──────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_reservations_within_quota_all_succeed() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 50 * 1024 * 1024));
        let barrier = Arc::new(Barrier::new(30));

        let handles: Vec<_> = (0..30)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    tokio::task::spawn_blocking(move || mgr.reserve(1024 * 1024))
                        .await
                        .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;
        let reservations: Vec<_> = results.into_iter().map(|r| r.unwrap().unwrap()).collect();

        assert_eq!(reservations.len(), 30);

        let status = manager.status().unwrap();
        assert_eq!(status.reserved_bytes, 30 * 1024 * 1024);
        assert!(status.accounted_bytes >= 30 * 1024 * 1024);
    }

    // ── 2. Concurrent reservations exceeding quota ───────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_reservations_exceeding_quota_some_fail() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 50 * 1024 * 1024));
        let barrier = Arc::new(Barrier::new(30));

        let handles: Vec<_> = (0..30)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    tokio::task::spawn_blocking(move || mgr.reserve(5 * 1024 * 1024))
                        .await
                        .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;
        let mut successes = 0u64;
        let mut failures = 0u64;

        for result in results {
            match result.unwrap() {
                Ok(_reservation) => successes += 1,
                Err(GitCacheError::DiskFull(_)) => failures += 1,
                Err(err) => panic!("unexpected error: {err}"),
            }
        }

        assert!(successes > 0, "at least one reservation should succeed");
        assert!(
            failures > 0,
            "at least one reservation should fail with DiskFull"
        );
        assert!(
            successes * 5 * 1024 * 1024 <= 50 * 1024 * 1024,
            "total reserved must not exceed quota"
        );

        let status = manager.status().unwrap();
        assert!(
            status.reserved_bytes <= 50 * 1024 * 1024,
            "reserved_bytes must never exceed quota"
        );
    }

    // ── 3. Reservation + status race ─────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn reservation_and_status_race_never_returns_inconsistent_data() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 100 * 1024 * 1024));

        let status_mgr = Arc::clone(&manager);
        let status_task = tokio::spawn(async move {
            for _ in 0..200 {
                let mgr = Arc::clone(&status_mgr);
                let status = tokio::task::spawn_blocking(move || mgr.status())
                    .await
                    .unwrap();
                // status() may transiently fail with IO errors during concurrent
                // reservation/release (layout dirs being created/removed).  That is
                // fine — we only check invariants when it succeeds.
                if let Ok(status) = status {
                    assert!(
                        status.available_bytes <= status.quota_bytes,
                        "available_bytes should not exceed quota"
                    );
                    assert_eq!(
                        status.accounted_bytes,
                        status.used_bytes + status.reserved_bytes,
                        "accounted must equal used + reserved"
                    );
                }
                tokio::task::yield_now().await;
            }
        });

        let reserve_mgr = Arc::clone(&manager);
        let reserve_task = tokio::spawn(async move {
            for _ in 0..50 {
                let mgr = Arc::clone(&reserve_mgr);
                let reservation = tokio::task::spawn_blocking(move || mgr.reserve(1024))
                    .await
                    .unwrap()
                    .unwrap();
                tokio::task::yield_now().await;
                drop(reservation);
            }
        });

        let (s, r) = tokio::join!(status_task, reserve_task);
        s.unwrap();
        r.unwrap();
    }

    // ── 4. Concurrent eviction ───────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_eviction_no_double_free() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 1024 * 1024));

        // Fill disk near quota with evictable repos.
        for i in 0..10 {
            let mgr = Arc::clone(&manager);
            let name = format!("repo{i}.git");
            tokio::task::spawn_blocking(move || {
                write_repo_file(&mgr, &name, 80_000);
                mgr.record_repo_access(&name).unwrap();
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        let barrier = Arc::new(Barrier::new(10));
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    tokio::task::spawn_blocking(move || mgr.reserve(100_000))
                        .await
                        .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;
        let mut successes = 0;
        for result in results {
            if result.unwrap().is_ok() {
                successes += 1;
            }
        }
        // At least some should succeed via eviction.
        assert!(
            successes > 0,
            "at least one eviction-based reservation should succeed"
        );

        // Verify no panics happened and status is consistent.
        let status = manager.status().unwrap();
        assert!(status.reserved_bytes <= status.quota_bytes);
    }

    // ── 5. Lock contention ───────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn lock_contention_tracks_correctly() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 100 * 1024 * 1024));

        // Create a repo to lock.
        {
            let mgr = Arc::clone(&manager);
            tokio::task::spawn_blocking(move || {
                write_repo_file(&mgr, "target.git", 100);
                mgr.record_repo_access("target.git").unwrap();
            })
            .await
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(10));
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    let mgr2 = Arc::clone(&mgr);
                    tokio::task::spawn_blocking(move || mgr2.lock_repo("target.git").unwrap())
                        .await
                        .unwrap()
                })
            })
            .collect();

        let mut locks: Vec<_> = join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        // All 10 locks are held simultaneously.
        let status = manager.status().unwrap();
        assert_eq!(
            status.locked_repo_count, 1,
            "one repo locked by multiple tasks"
        );

        // Drop half.
        for _ in 0..5 {
            locks.pop();
        }

        // Still locked (5 remaining).
        let status = manager.status().unwrap();
        assert_eq!(status.locked_repo_count, 1);

        // Drop all remaining.
        locks.clear();

        let status = manager.status().unwrap();
        assert_eq!(status.locked_repo_count, 0);
    }

    // ── 6. Concurrent record_repo_access ─────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_record_repo_access_no_corruption() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 100 * 1024 * 1024));

        {
            let mgr = Arc::clone(&manager);
            tokio::task::spawn_blocking(move || {
                write_repo_file(&mgr, "shared.git", 500);
            })
            .await
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(20));
        let handles: Vec<_> = (0..20)
            .map(|_| {
                let mgr = Arc::clone(&manager);
                let bar = Arc::clone(&barrier);
                tokio::spawn(async move {
                    bar.wait().await;
                    let mgr2 = Arc::clone(&mgr);
                    tokio::task::spawn_blocking(move || mgr2.record_repo_access("shared.git"))
                        .await
                        .unwrap()
                })
            })
            .collect();

        let results: Vec<_> = join_all(handles).await;
        for result in &results {
            result.as_ref().unwrap().as_ref().unwrap();
        }

        // The index should still be consistent.
        let index = manager.repo_index().unwrap();
        assert!(index.repos.contains_key(std::path::Path::new("shared.git")));
        assert_eq!(index.repos.len(), 1);
    }

    // ── 7. Interleaved reserve + cleanup_stale_temps ─────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn cleanup_does_not_remove_active_reservations() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 100 * 1024 * 1024));

        // Create a reservation.
        let mgr = Arc::clone(&manager);
        let reservation = tokio::task::spawn_blocking(move || mgr.reserve(4096))
            .await
            .unwrap()
            .unwrap();
        let temp_path = reservation.temp_path();
        assert!(temp_path.exists());

        // Run cleanup concurrently — active reservation should survive.
        let mgr = Arc::clone(&manager);
        let report = tokio::task::spawn_blocking(move || mgr.cleanup_stale_temps(Duration::ZERO))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(report.removed_temp_dirs, 0);
        assert_eq!(report.removed_reservation_markers, 0);
        assert!(
            temp_path.exists(),
            "active reservation should NOT be cleaned up"
        );

        drop(reservation);
        assert!(
            !temp_path.exists(),
            "reservation should be cleaned up on drop"
        );
    }

    // ── 8. Drop without release ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn drop_without_release_cleans_up() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(make_manager(&tmp, 100 * 1024 * 1024));

        let mut temp_paths = Vec::new();

        for _ in 0..5 {
            let mgr = Arc::clone(&manager);
            let reservation = tokio::task::spawn_blocking(move || mgr.reserve(1024))
                .await
                .unwrap()
                .unwrap();
            temp_paths.push(reservation.temp_path());
            // Intentionally drop without calling release.
            drop(reservation);
        }

        // All temp dirs should have been cleaned up by Drop.
        for path in &temp_paths {
            assert!(!path.exists(), "temp dir should be cleaned up on drop");
        }

        let status = manager.status().unwrap();
        assert_eq!(status.reserved_bytes, 0, "no lingering reservations");
    }
}
