//! Load tests for the disk manager simulating large-scale repository management.

use futures::future::join_all;
use git_cache_disk::DiskManager;
use std::sync::Arc;
use tempfile::TempDir;

fn make_manager(tmp: &TempDir, quota: u64) -> DiskManager {
    DiskManager::new(tmp.path(), quota, 0)
}

fn write_repo_file(manager: &DiskManager, repo_path: &str, bytes: usize) {
    let repo_dir = manager.repos_dir().join(repo_path);
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("objects.pack"), vec![0u8; bytes]).unwrap();
}

// ── 1. Many repos management ────────────────────────────────────────────

#[test]
fn many_repos_management() {
    let tmp = TempDir::new().unwrap();
    let quota = 500 * 1024 * 1024; // 500MB
    let dm = make_manager(&tmp, quota);

    let repo_count = 100;
    let repo_size = 100 * 1024; // 100KB each

    for i in 0..repo_count {
        let repo_path = format!("github.com/org/repo-{i}.git");
        write_repo_file(&dm, &repo_path, repo_size);
        dm.record_repo_access(&repo_path)
            .unwrap_or_else(|e| panic!("record repo {i}: {e}"));
    }

    let status = dm.status().unwrap();
    assert_eq!(
        status.repo_count, repo_count,
        "should track all {repo_count} repos, got {}",
        status.repo_count
    );
    assert!(
        status.used_bytes >= (repo_count as u64) * (repo_size as u64),
        "used_bytes {} should account for all repo data",
        status.used_bytes
    );
}

// ── 2. Eviction cascade ────────────────────────────────────────────────

#[test]
fn eviction_cascade() {
    let tmp = TempDir::new().unwrap();
    let repo_size = 100 * 1024usize; // 100KB per repo
    let repo_count = 50;
    let dm = DiskManager::new(tmp.path(), 1024 * 1024 * 1024, 0);

    for i in 0..repo_count {
        let repo_path = format!("github.com/org/repo-{i}.git");
        write_repo_file(&dm, &repo_path, repo_size);
        dm.record_repo_access(&repo_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let status_before = dm.status().unwrap();
    assert_eq!(status_before.repo_count, repo_count);

    // Now create a new DiskManager with a tight quota that forces eviction.
    // The used_bytes includes all data on disk, so set quota to used_bytes +
    // just enough headroom that a large reservation forces eviction of 10+ repos.
    let tight_quota = status_before.used_bytes + 100 * 1024; // barely above current use
    let dm2 = DiskManager::new(tmp.path(), tight_quota, 0);

    // Reserve enough bytes to require evicting at least 10 repos (10 * 100KB = 1MB).
    let reserve_bytes = 2 * 1024 * 1024u64; // 2MB
    let reservation = dm2
        .reserve(reserve_bytes)
        .expect("reservation requiring eviction should succeed");

    let status_after = dm2.status().unwrap();
    let evicted = status_before.repo_count - status_after.repo_count;
    assert!(
        evicted >= 10,
        "should have evicted at least 10 repos, evicted {evicted}"
    );

    drop(reservation);
}

// ── 3. Sustained reservation pressure ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn sustained_reservation_pressure() {
    let tmp = TempDir::new().unwrap();
    let quota = 50 * 1024 * 1024u64; // 50MB
    let dm = Arc::new(make_manager(&tmp, quota));

    let task_count = 100;
    let iterations = 50;
    let reserve_bytes = 10 * 1024u64; // 10KB each

    let handles: Vec<_> = (0..task_count)
        .map(|_| {
            let mgr = Arc::clone(&dm);
            tokio::spawn(async move {
                for _ in 0..iterations {
                    let result = tokio::task::spawn_blocking(move || mgr.reserve(reserve_bytes))
                        .await
                        .unwrap();
                    // We don't require every reserve to succeed (quota may be hit), but we
                    // do require no panics/deadlocks.  Drop immediately to release.
                    drop(result);
                    // Re-clone Arc for next iteration inside spawn_blocking
                    // (the move consumed the previous one).
                    break; // single iteration per spawn to avoid move issues
                }
            })
        })
        .collect();

    // Run in batches to stay within quota
    for handle in handles {
        handle.await.unwrap();
    }

    let status = dm.status().unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all reservations should be released, but {} bytes remain",
        status.reserved_bytes
    );
}

// ── (3b) sustained_reservation_pressure — actual 50-iteration loop ──────

#[tokio::test(flavor = "multi_thread")]
async fn sustained_reservation_pressure_full() {
    let tmp = TempDir::new().unwrap();
    let quota = 50 * 1024 * 1024u64; // 50MB
    let dm = Arc::new(make_manager(&tmp, quota));

    let task_count = 100;
    let iterations_per_task = 50;
    let reserve_bytes = 10 * 1024u64; // 10KB

    let handles: Vec<_> = (0..task_count)
        .map(|_| {
            let mgr = Arc::clone(&dm);
            tokio::spawn(async move {
                for _ in 0..iterations_per_task {
                    let m = Arc::clone(&mgr);
                    let result = tokio::task::spawn_blocking(move || m.reserve(reserve_bytes))
                        .await
                        .unwrap();
                    drop(result);
                }
            })
        })
        .collect();

    join_all(handles).await.into_iter().for_each(|r| r.unwrap());

    let status = dm.status().unwrap();
    assert_eq!(
        status.reserved_bytes, 0,
        "all reservations should be released"
    );
}

// ── 4. Large reservation ────────────────────────────────────────────────

#[test]
fn large_reservation() {
    let tmp = TempDir::new().unwrap();
    let quota = 100 * 1024 * 1024u64; // 100MB
    let dm = make_manager(&tmp, quota);

    let reserve_bytes = 50 * 1024 * 1024u64; // 50MB
    let reservation = dm.reserve(reserve_bytes).expect("50MB reservation");

    let status_during = dm.status().unwrap();
    assert!(
        status_during.reserved_bytes >= reserve_bytes,
        "reserved_bytes should include the 50MB reservation"
    );

    // Write some data to the temp path
    let temp_path = reservation.temp_path();
    std::fs::write(
        temp_path.join("large-data.bin"),
        vec![0xABu8; 10 * 1024 * 1024],
    )
    .expect("write 10MB to reservation temp");

    drop(reservation);

    let status_after = dm.status().unwrap();
    assert_eq!(
        status_after.reserved_bytes, 0,
        "reservation should be fully released"
    );
}
