//! Randomized concurrent fuzzing of `DiskManager` / `AsyncDiskManager`.
//!
//! Many tasks race reservations, repo locks, invalidation, LRU eviction,
//! index updates, and stale-temp cleanup against a deliberately tiny quota
//! so eviction pressure is constant. Operations may fail with `Conflict` or
//! disk-full errors — that is expected; what must never happen is a panic,
//! a deadlock (deadline timeout), or leaked lock/reservation accounting.

use git_cache_disk::{AsyncDiskManager, DiskManager};
use git_cache_fuzz::FuzzConfig;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const REPO_POOL: usize = 8;

fn repo_rel_path(i: usize) -> PathBuf {
    PathBuf::from(format!("github.com/fuzz/repo-{i}.git"))
}

fn populate_repo(disk: &DiskManager, i: usize, bytes: usize) -> std::io::Result<()> {
    let dir = disk.repos_dir().join(repo_rel_path(i));
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("pack"), vec![0u8; bytes])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn disk_manager_survives_randomized_concurrent_ops() {
    let config = FuzzConfig::from_env("disk_fuzz", 12, 60);
    let tmp = tempfile::tempdir().expect("tempdir");
    // Tiny quota (64 KiB) so reservations and repo growth constantly trigger
    // the LRU eviction path while locks and invalidations race it.
    let disk = DiskManager::new(tmp.path(), 64 * 1024, 0);
    let async_disk = AsyncDiskManager::new(disk.clone());

    let run = async {
        let mut handles = Vec::new();
        for task in 0..config.tasks {
            let mut rng = config.task_rng(task);
            let disk = disk.clone();
            let async_disk = async_disk.clone();
            let ops = config.ops_per_task;

            handles.push(tokio::spawn(async move {
                for _ in 0..ops {
                    let repo = rng.usize(..REPO_POOL);
                    match rng.u32(..100) {
                        // Reserve scratch space, optionally write into it,
                        // then release (or drop, exercising Drop cleanup).
                        0..=19 => {
                            let bytes = rng.u64(1..16 * 1024);
                            let release = rng.bool();
                            if let Ok(reservation) = async_disk.reserve(bytes).await {
                                if let Ok(temp) = reservation.temp_path() {
                                    let _ = fs::write(
                                        temp.join("scratch"),
                                        vec![0u8; rng.usize(..4096)],
                                    );
                                }
                                if release {
                                    let _ = reservation.release().await;
                                }
                            }
                        }
                        // Hold a repo lock across an await point.
                        20..=39 => {
                            if let Ok(lock) = async_disk.lock_repo(repo_rel_path(repo)).await {
                                tokio::time::sleep(Duration::from_micros(rng.u64(..500))).await;
                                drop(lock);
                            }
                        }
                        // Invalidate (races locks; Conflict expected).
                        40..=49 => {
                            let _ = async_disk.invalidate_repo(repo_rel_path(repo)).await;
                        }
                        // Grow a repo and record it (forces directory walks
                        // and index writes under the state mutex).
                        50..=69 => {
                            let bytes = rng.usize(..8 * 1024);
                            let disk = disk.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                populate_repo(&disk, repo, bytes)?;
                                disk.record_repo_access(repo_rel_path(repo))
                                    .map_err(std::io::Error::other)
                            })
                            .await;
                        }
                        70..=79 => {
                            let _ = async_disk.touch_repo_access(repo_rel_path(repo)).await;
                        }
                        80..=89 => {
                            let _ = async_disk.status().await;
                        }
                        90..=94 => {
                            let _ = async_disk.repo_index().await;
                        }
                        _ => {
                            let _ = async_disk.cleanup_stale_temps(Duration::ZERO).await;
                        }
                    }
                }
            }));
        }

        for handle in handles {
            handle.await.expect("fuzz task must not panic");
        }
    };

    tokio::time::timeout(config.deadline, run)
        .await
        .expect("disk fuzz deadlocked: tasks did not finish before the deadline");

    // No leaked accounting: with all locks/reservations dropped, the manager
    // must report zero locked repos and zero reserved bytes.
    let status = async_disk.status().await.expect("status after fuzz");
    assert_eq!(status.locked_repo_count, 0, "leaked repo locks: {status:?}");
    assert_eq!(status.reserved_bytes, 0, "leaked reservations: {status:?}");
    assert!(
        status.used_bytes <= status.quota_bytes,
        "quota exceeded after fuzz: {status:?}"
    );

    // The manager must still be fully functional after the storm.
    populate_repo(&disk, 0, 128).expect("repopulate");
    disk.record_repo_access(repo_rel_path(0))
        .expect("record after fuzz");
    let reservation = async_disk.reserve(1024).await.expect("reserve after fuzz");
    reservation.release().await.expect("release after fuzz");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn repo_locks_block_eviction_and_invalidation_under_pressure() {
    let config = FuzzConfig::from_env("disk_lock_fuzz", 8, 40);
    let tmp = tempfile::tempdir().expect("tempdir");
    let disk = DiskManager::new(tmp.path(), 48 * 1024, 0);
    let async_disk = AsyncDiskManager::new(disk.clone());

    // One protected repo that must survive all eviction pressure.
    populate_repo(&disk, 0, 4 * 1024).expect("populate pinned");
    disk.record_repo_access(repo_rel_path(0))
        .expect("record pinned");
    let pinned = Arc::new(
        async_disk
            .lock_repo(repo_rel_path(0))
            .await
            .expect("lock pinned repo"),
    );

    let run = async {
        let mut handles = Vec::new();
        for task in 0..config.tasks {
            let mut rng = config.task_rng(task);
            let disk = disk.clone();
            let async_disk = async_disk.clone();
            let ops = config.ops_per_task;

            handles.push(tokio::spawn(async move {
                for _ in 0..ops {
                    let repo = rng.usize(1..REPO_POOL);
                    match rng.u32(..100) {
                        // Churn unpinned repos to drive eviction.
                        0..=49 => {
                            let bytes = rng.usize(1024..12 * 1024);
                            let disk = disk.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                populate_repo(&disk, repo, bytes)?;
                                disk.record_repo_access(repo_rel_path(repo))
                                    .map_err(std::io::Error::other)
                            })
                            .await;
                        }
                        // Reservations force evict_until_available.
                        50..=79 => {
                            if let Ok(reservation) = async_disk.reserve(rng.u64(1..24 * 1024)).await
                            {
                                let _ = reservation.release().await;
                            }
                        }
                        // Try to invalidate the pinned repo: must always fail
                        // with Conflict while the lock is held.
                        _ => {
                            let result = async_disk.invalidate_repo(repo_rel_path(0)).await;
                            assert!(
                                result.is_err(),
                                "invalidate of a locked repo must be rejected"
                            );
                        }
                    }
                }
            }));
        }

        for handle in handles {
            handle.await.expect("fuzz task must not panic");
        }
    };

    tokio::time::timeout(config.deadline, run)
        .await
        .expect("disk lock fuzz deadlocked");

    // The locked repo must never have been evicted.
    assert!(
        disk.repos_dir().join(repo_rel_path(0)).exists(),
        "locked repo was evicted under pressure"
    );

    drop(Arc::try_unwrap(pinned).ok());
    let status = async_disk.status().await.expect("status after fuzz");
    assert_eq!(status.locked_repo_count, 0, "leaked repo locks: {status:?}");
    // Once unlocked, invalidation must succeed again.
    async_disk
        .invalidate_repo(repo_rel_path(0))
        .await
        .expect("invalidate after unlock");
}
