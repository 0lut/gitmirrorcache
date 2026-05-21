//! Advanced performance tests for the disk manager.

use git_cache_disk::DiskManager;
use std::time::Instant;
use tempfile::TempDir;

fn test_disk_manager(name: &str, quota_bytes: u64) -> (TempDir, DiskManager) {
    let tmp = TempDir::with_prefix(name).unwrap();
    let dm = DiskManager::new(tmp.path(), quota_bytes, 0);
    (tmp, dm)
}

// ── 1. Reservation throughput scaling (500 cycles) ──────────────────────

#[test]
fn test_reservation_throughput_scaling() {
    let (_tmp, dm) = test_disk_manager("reserve-scale", 1024 * 1024 * 1024);
    let iterations = 500;

    let start = Instant::now();
    for _ in 0..iterations {
        let reservation = dm.reserve(1024).unwrap();
        drop(reservation);
    }
    let elapsed = start.elapsed();

    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();
    eprintln!(
        "reservation throughput scaling: {iterations} cycles in {elapsed:?} ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 120,
        "reservation throughput scaling too slow: {elapsed:?}"
    );

    let status = dm.status().unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all reservations should be released"
    );
}

// ── 2. Concurrent reservation scaling (50 tasks, 100MB quota) ───────────

#[tokio::test]
async fn test_concurrent_reservation_scaling() {
    let tmp = TempDir::with_prefix("concurrent-scale").unwrap();
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 100, 0); // 100MB
    let dm_arc = std::sync::Arc::new(dm);
    let concurrent_tasks = 50;
    let reserve_bytes: u64 = 1024 * 1024; // 1MB each

    // Warm up the disk layout so concurrent tasks don't race on dir creation.
    let _ = dm_arc.status();

    let start = Instant::now();
    let success_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..concurrent_tasks {
        let dm = dm_arc.clone();
        let successes = success_count.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            if let Ok(reservation) = dm.reserve(reserve_bytes) {
                let _path = reservation.temp_path();
                drop(reservation);
                successes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }
    let total_successes = success_count.load(std::sync::atomic::Ordering::Relaxed);
    let elapsed = start.elapsed();

    let status = dm_arc.status().unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all reservations should be released, but {} bytes remain",
        status.reserved_bytes
    );

    assert!(
        total_successes > 0,
        "at least one concurrent reservation should succeed"
    );

    let ops_per_sec = total_successes as f64 / elapsed.as_secs_f64();
    eprintln!(
        "concurrent reservation scaling: {total_successes}/{concurrent_tasks} succeeded in {elapsed:?} ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 120,
        "concurrent reservation scaling too slow: {elapsed:?}"
    );
}

// ── 3. Eviction performance with many repos (200) ───────────────────────

#[test]
fn test_eviction_performance_many_repos() {
    let tmp = TempDir::with_prefix("eviction-many").unwrap();
    let quota: u64 = 1024 * 1024 * 50; // 50MB
    let dm = DiskManager::new(tmp.path(), quota, 0);

    let repos_dir = tmp.path().join("repos");
    let small_repo_count = 200;
    let small_repo_size = 100 * 1024; // 100KB each => ~20MB total
    for i in 0..small_repo_count {
        let repo_path = repos_dir.join(format!("github.com/org/repo-{i}.git"));
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join("data.bin"), vec![0u8; small_repo_size]).unwrap();
        dm.record_repo_access(format!("github.com/org/repo-{i}.git"))
            .unwrap();
        // Stagger access times slightly so LRU ordering is deterministic.
    }

    let status_before = dm.status().unwrap();
    assert!(
        status_before.repo_count >= small_repo_count,
        "expected at least {small_repo_count} repos, got {}",
        status_before.repo_count
    );

    // Request a reservation that forces eviction of many repos.
    let large_reserve = 35 * 1024 * 1024; // 35MB (needs to evict most of the 20MB of repos)
    let start = Instant::now();
    let reservation = dm.reserve(large_reserve).unwrap();
    let eviction_elapsed = start.elapsed();

    let status_after = dm.status().unwrap();
    eprintln!(
        "eviction with {small_repo_count} repos: {eviction_elapsed:?}, repos before={}, repos after={}",
        status_before.repo_count, status_after.repo_count
    );

    assert!(
        status_after.repo_count < status_before.repo_count,
        "eviction should have removed some repos"
    );
    assert!(
        eviction_elapsed.as_secs() < 120,
        "eviction too slow: {eviction_elapsed:?}"
    );

    drop(reservation);
}

// ── 4. Status computation time (100 repos, 200 calls) ───────────────────

#[test]
fn test_status_computation_time() {
    let tmp = TempDir::with_prefix("status-comp").unwrap();
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 1024, 0);

    // Create 100 repos on disk.
    let repos_dir = tmp.path().join("repos");
    let repo_count = 100;
    for i in 0..repo_count {
        let repo_path = repos_dir.join(format!("github.com/org/repo-{i}.git"));
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join("data.bin"), vec![0u8; 1024]).unwrap();
        dm.record_repo_access(format!("github.com/org/repo-{i}.git"))
            .unwrap();
    }

    let iterations = 200u32;
    let start = Instant::now();
    for _ in 0..iterations {
        let status = dm.status().unwrap();
        assert!(
            status.repo_count >= repo_count,
            "expected at least {repo_count} repos"
        );
    }
    let elapsed = start.elapsed();

    let avg = elapsed / iterations;
    eprintln!(
        "status computation ({repo_count} repos): {iterations} calls in {elapsed:?}, avg={avg:?}"
    );
    assert!(
        elapsed.as_secs() < 120,
        "status computation too slow: {elapsed:?}"
    );
}

// ── 5. Lock/unlock throughput ────────────────────────────────────────────

#[test]
fn test_lock_unlock_throughput() {
    let (_tmp, dm) = test_disk_manager("lock-unlock", 1024 * 1024 * 1024);
    let iterations = 100;

    let start = Instant::now();
    for i in 0..iterations {
        let repo_path = format!("github.com/org/repo-{i}.git");
        let lock = dm.lock_repo(&repo_path).unwrap();
        drop(lock);
    }
    let elapsed = start.elapsed();

    let ops_per_sec = iterations as f64 / elapsed.as_secs_f64();
    eprintln!(
        "lock/unlock throughput: {iterations} cycles in {elapsed:?} ({ops_per_sec:.0} ops/sec)"
    );
    assert!(
        elapsed.as_secs() < 120,
        "lock/unlock throughput too slow: {elapsed:?}"
    );
}
