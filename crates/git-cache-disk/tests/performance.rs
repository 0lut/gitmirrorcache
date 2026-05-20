//! Performance / throughput tests for the disk manager.

use git_cache_disk::{AsyncDiskManager, DiskManager};
use std::time::Instant;
use tempfile::TempDir;

fn test_disk_manager(name: &str, quota_bytes: u64) -> (TempDir, DiskManager) {
    let tmp = TempDir::with_prefix(name).unwrap();
    let dm = DiskManager::new(tmp.path(), quota_bytes, 0);
    (tmp, dm)
}

// ── 1. Rapid reservation/release cycle ───────────────────────────────────

#[test]
fn test_reservation_rapid_cycle() {
    let (_tmp, dm) = test_disk_manager("rapid-cycle", 1024 * 1024 * 1024);
    let iterations = 100;

    let start = Instant::now();
    for _ in 0..iterations {
        let reservation = dm.reserve(1024).unwrap();
        drop(reservation);
    }
    let elapsed = start.elapsed();

    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();
    eprintln!(
        "rapid reserve/release: {iterations} cycles in {elapsed:?} ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 30,
        "rapid reserve/release too slow: {elapsed:?}"
    );

    let status = dm.status().unwrap();
    assert_eq!(status.reserved_bytes, 0, "all reservations should be released");
}

// ── 2. Concurrent reservations ───────────────────────────────────────────

#[tokio::test]
async fn test_concurrent_reservations() {
    let tmp = TempDir::with_prefix("concurrent-reserve").unwrap();
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 100, 0); // 100MB quota
    let concurrent_tasks = 20;
    let reserve_bytes: u64 = 1024 * 1024; // 1MB each

    let start = Instant::now();
    let dm_arc = std::sync::Arc::new(dm);
    let mut handles = Vec::new();
    for _ in 0..concurrent_tasks {
        let dm = dm_arc.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let reservation = dm.reserve(reserve_bytes).unwrap();
            let _path = reservation.temp_path();
            // Hold briefly then release.
            drop(reservation);
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
    let elapsed = start.elapsed();

    let status = dm_arc.status().unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all reservations should be released, but {} bytes remain",
        status.reserved_bytes
    );

    eprintln!(
        "concurrent reservations: {concurrent_tasks} tasks in {elapsed:?}"
    );
    assert!(
        elapsed.as_secs() < 30,
        "concurrent reservations too slow: {elapsed:?}"
    );
}

// ── 3. Eviction under pressure ───────────────────────────────────────────

#[test]
fn test_eviction_under_pressure() {
    let tmp = TempDir::with_prefix("eviction-pressure").unwrap();
    let quota: u64 = 1024 * 1024 * 10; // 10MB quota
    let dm = DiskManager::new(tmp.path(), quota, 0);

    // Fill with many small repos to make them evictable.
    let repos_dir = tmp.path().join("repos");
    let small_repo_count = 50;
    let small_repo_size = 100 * 1024; // 100KB each (total ~5MB)
    for i in 0..small_repo_count {
        let repo_path = repos_dir.join(format!("github.com/org/repo-{i}.git"));
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join("data.bin"), vec![0u8; small_repo_size]).unwrap();
        dm.record_repo_access(format!("github.com/org/repo-{i}.git"))
            .unwrap();
    }

    let status_before = dm.status().unwrap();
    assert!(
        status_before.repo_count >= small_repo_count,
        "expected at least {small_repo_count} repos, got {}",
        status_before.repo_count
    );

    // Request a large reservation that requires eviction.
    let large_reserve = 6 * 1024 * 1024; // 6MB
    let start = Instant::now();
    let reservation = dm.reserve(large_reserve).unwrap();
    let eviction_elapsed = start.elapsed();

    let status_after = dm.status().unwrap();
    eprintln!(
        "eviction under pressure: {eviction_elapsed:?}, repos before={}, repos after={}",
        status_before.repo_count, status_after.repo_count
    );

    // Some repos should have been evicted.
    assert!(
        status_after.repo_count < status_before.repo_count,
        "eviction should have removed some repos"
    );
    assert!(
        eviction_elapsed.as_secs() < 30,
        "eviction too slow: {eviction_elapsed:?}"
    );

    drop(reservation);
}

// ── 4. AsyncDiskManager rapid cycle ──────────────────────────────────────

#[tokio::test]
async fn test_async_disk_manager_rapid_cycle() {
    let tmp = TempDir::with_prefix("async-rapid-cycle").unwrap();
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 1024, 0);
    let async_dm = AsyncDiskManager::new(dm);
    let iterations = 100;

    let start = Instant::now();
    for _ in 0..iterations {
        let reservation = async_dm.reserve(1024).await.unwrap();
        reservation.release().await.unwrap();
    }
    let elapsed = start.elapsed();

    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();
    eprintln!(
        "async rapid reserve/release: {iterations} cycles in {elapsed:?} ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 60,
        "async rapid reserve/release too slow: {elapsed:?}"
    );

    let status = async_dm.status().await.unwrap();
    assert_eq!(status.reserved_bytes, 0);
}

// ── 5. AsyncDiskManager concurrent reservations ──────────────────────────

#[tokio::test]
async fn test_async_concurrent_reservations() {
    let tmp = TempDir::with_prefix("async-concurrent").unwrap();
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 100, 0);
    let async_dm = AsyncDiskManager::new(dm);
    let concurrent_tasks = 20;
    let reserve_bytes: u64 = 1024 * 1024;

    // Warm up the layout directories before concurrent access.
    let warmup = async_dm.reserve(1).await.unwrap();
    warmup.release().await.unwrap();

    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..concurrent_tasks {
        let async_dm = async_dm.clone();
        handles.push(tokio::spawn(async move {
            match async_dm.reserve(reserve_bytes).await {
                Ok(reservation) => {
                    let _ = reservation.release().await;
                }
                Err(_) => {
                    // Transient IO errors (layout dirs being created) are acceptable
                    // under concurrent access.
                }
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
    let elapsed = start.elapsed();

    let status = async_dm.status().await.unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all async reservations should be released"
    );

    eprintln!(
        "async concurrent reservations: {concurrent_tasks} tasks in {elapsed:?}"
    );
    assert!(
        elapsed.as_secs() < 60,
        "async concurrent reservations too slow: {elapsed:?}"
    );
}

// ── 6. AsyncDiskManager eviction under pressure ──────────────────────────

#[tokio::test]
async fn test_async_eviction_under_pressure() {
    let tmp = TempDir::with_prefix("async-eviction").unwrap();
    let quota: u64 = 1024 * 1024 * 10;
    let dm = DiskManager::new(tmp.path(), quota, 0);
    let async_dm = AsyncDiskManager::new(dm);

    let repos_dir = tmp.path().join("repos");
    let small_repo_count = 50;
    let small_repo_size = 100 * 1024;
    for i in 0..small_repo_count {
        let repo_path = repos_dir.join(format!("github.com/org/repo-{i}.git"));
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join("data.bin"), vec![0u8; small_repo_size]).unwrap();
        async_dm
            .record_repo_access(format!("github.com/org/repo-{i}.git").into())
            .await
            .unwrap();
    }

    let status_before = async_dm.status().await.unwrap();

    let large_reserve: u64 = 6 * 1024 * 1024;
    let start = Instant::now();
    let reservation = async_dm.reserve(large_reserve).await.unwrap();
    let eviction_elapsed = start.elapsed();

    let status_after = async_dm.status().await.unwrap();
    eprintln!(
        "async eviction: {eviction_elapsed:?}, repos before={}, repos after={}",
        status_before.repo_count, status_after.repo_count
    );

    assert!(
        status_after.repo_count < status_before.repo_count,
        "async eviction should have removed some repos"
    );
    assert!(
        eviction_elapsed.as_secs() < 30,
        "async eviction too slow: {eviction_elapsed:?}"
    );

    reservation.release().await.unwrap();
}
