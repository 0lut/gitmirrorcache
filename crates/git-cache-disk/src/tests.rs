use super::*;
use tempfile::tempdir;

#[test]
fn reservation_success_creates_temp_dir_and_marker() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);

    let reservation = manager.reserve(512).expect("reservation");
    assert!(reservation.temp_path().is_dir());
    assert!(reservation.marker_path().is_file());

    let status = manager.status().expect("status");
    assert_eq!(status.reserved_bytes, 512);
    assert!(status.accounted_bytes >= 512);

    let temp_path = reservation.temp_path();
    let marker_path = reservation.marker_path();
    drop(reservation);

    assert!(!temp_path.exists());
    assert!(!marker_path.exists());
    assert_eq!(manager.status().expect("status").reserved_bytes, 0);
}

#[test]
fn reservation_returns_disk_full_when_nothing_can_be_evicted() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 100, 0);
    fs::create_dir_all(root.path()).expect("root");
    fs::write(root.path().join("payload"), vec![0u8; 80]).expect("payload");

    let err = manager.reserve(30).expect_err("disk full");
    assert!(matches!(err, GitCacheError::DiskFull(_)));
}

#[test]
fn reserve_evicts_unlocked_lru_repo_until_it_fits() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 900, 0);
    write_repo_file(&manager, "old.git", 1_000);
    write_repo_file(&manager, "new.git", 100);

    manager.record_repo_access("old.git").expect("old access");
    std::thread::sleep(Duration::from_millis(2));
    manager.record_repo_access("new.git").expect("new access");

    let reservation = manager.reserve(300).expect("reservation");

    assert!(!manager.repos_dir().join("old.git").exists());
    assert!(manager.repos_dir().join("new.git").exists());
    assert_eq!(reservation.bytes(), 300);
}

#[test]
fn note_repo_access_is_invisible_until_flush_then_persisted() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "repo.git", 10);
    manager.record_repo_access("repo.git").expect("record");
    let before =
        manager.repo_index().expect("index").repos[Path::new("repo.git")].last_accessed_unix_millis;

    std::thread::sleep(Duration::from_millis(2));
    manager.note_repo_access("repo.git").expect("note");
    let unflushed =
        manager.repo_index().expect("index").repos[Path::new("repo.git")].last_accessed_unix_millis;
    assert_eq!(unflushed, before);

    assert_eq!(manager.flush_repo_accesses().expect("flush"), 1);
    let flushed =
        manager.repo_index().expect("index").repos[Path::new("repo.git")].last_accessed_unix_millis;
    assert!(flushed > before);

    // A second flush with nothing pending is a no-op.
    assert_eq!(manager.flush_repo_accesses().expect("flush"), 0);

    // The flushed timestamp survives a fresh manager (process restart).
    let reopened = DiskManager::new(root.path(), 10_000, 0);
    let restarted = reopened.repo_index().expect("index").repos[Path::new("repo.git")]
        .last_accessed_unix_millis;
    assert_eq!(restarted, flushed);
}

#[test]
fn flush_indexes_unknown_repo_dir_and_skips_missing_dirs() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "present.git", 10);

    manager.note_repo_access("present.git").expect("note");
    manager.note_repo_access("missing.git").expect("note");

    assert_eq!(manager.flush_repo_accesses().expect("flush"), 1);
    let index = manager.repo_index().expect("index");
    assert!(index.repos.contains_key(Path::new("present.git")));
    assert!(!index.repos.contains_key(Path::new("missing.git")));
}

#[test]
fn eviction_consults_pending_accesses_before_picking_victim() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 900, 0);
    write_repo_file(&manager, "a.git", 300);
    write_repo_file(&manager, "b.git", 500);

    manager.record_repo_access("a.git").expect("a access");
    std::thread::sleep(Duration::from_millis(2));
    manager.record_repo_access("b.git").expect("b access");

    // `a` is the persisted LRU victim, but an unflushed access makes it
    // the most recently used; eviction must pick `b` instead.
    std::thread::sleep(Duration::from_millis(2));
    manager.note_repo_access("a.git").expect("note");

    manager.reserve(300).expect("reservation");

    assert!(manager.repos_dir().join("a.git").exists());
    assert!(!manager.repos_dir().join("b.git").exists());
}

#[test]
fn invalidate_repo_drops_pending_access() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "repo.git", 10);
    manager.record_repo_access("repo.git").expect("record");
    manager.note_repo_access("repo.git").expect("note");

    manager.invalidate_repo("repo.git").expect("invalidate");

    assert_eq!(manager.flush_repo_accesses().expect("flush"), 0);
    assert!(!manager
        .repo_index()
        .expect("index")
        .repos
        .contains_key(Path::new("repo.git")));
}

#[test]
fn eviction_skips_locked_repos() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 900, 0);
    write_repo_file(&manager, "old.git", 300);
    write_repo_file(&manager, "new.git", 500);

    manager.record_repo_access("old.git").expect("old access");
    std::thread::sleep(Duration::from_millis(2));
    manager.record_repo_access("new.git").expect("new access");
    let _lock = manager.lock_repo("old.git").expect("repo lock");

    manager.reserve(300).expect("reservation");

    assert!(manager.repos_dir().join("old.git").exists());
    assert!(!manager.repos_dir().join("new.git").exists());
}

#[test]
fn protected_repos_are_not_evicted() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 100, 0);
    write_repo_file(&manager, "protected.git", 80);
    manager
        .set_repo_protected("protected.git", true)
        .expect("protect");

    let err = manager.reserve(30).expect_err("disk full");
    assert!(matches!(err, GitCacheError::DiskFull(_)));
    assert!(manager.repos_dir().join("protected.git").exists());
}

#[test]
fn cleanup_removes_stale_temp_dirs_and_reservation_markers() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    manager.ensure_layout().expect("layout");

    let stale_id = Uuid::now_v7();
    let temp_dir = manager.temp_dir_for(stale_id);
    fs::create_dir_all(&temp_dir).expect("temp dir");
    fs::write(temp_dir.join("pack.tmp"), vec![0u8; 16]).expect("tmp file");
    let orphan_id = Uuid::now_v7();
    let orphan_temp_dir = manager.temp_dir_for(orphan_id);
    fs::create_dir_all(&orphan_temp_dir).expect("orphan temp dir");
    fs::write(orphan_temp_dir.join("pack.tmp"), vec![0u8; 8]).expect("orphan tmp file");
    let named_temp_dir = manager.tmp_dir().join("interrupted-verification.git");
    fs::create_dir_all(&named_temp_dir).expect("named temp dir");
    fs::write(named_temp_dir.join("pack.tmp"), vec![0u8; 4]).expect("named tmp file");

    let marker = ReservationMarker {
        id: stale_id,
        bytes: 128,
        temp_dir: temp_dir.clone(),
        created_at_unix_millis: now_unix_millis(),
    };
    write_json(&manager.marker_path(stale_id), &marker).expect("marker");

    let report = manager
        .cleanup_stale_temps(Duration::ZERO)
        .expect("cleanup");

    assert_eq!(report.removed_temp_dirs, 3);
    assert_eq!(report.removed_reservation_markers, 1);
    assert!(report.freed_bytes >= 28);
    assert!(!temp_dir.exists());
    assert!(!orphan_temp_dir.exists());
    assert!(!named_temp_dir.exists());
    assert!(!manager.marker_path(stale_id).exists());
}

#[test]
fn status_accounts_root_size_and_on_disk_reservations() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 1_000, 100);
    write_repo_file(&manager, "repo.git", 20);
    manager.record_repo_access("repo.git").expect("access");
    manager
        .set_repo_protected("repo.git", true)
        .expect("protect");
    let reservation = manager.reserve(30).expect("reservation");

    let second_manager = DiskManager::new(root.path(), 1_000, 100);
    let status = second_manager.status().expect("status");

    assert_eq!(status.reserved_bytes, 30);
    assert_eq!(status.accounted_bytes, status.used_bytes + 30);
    assert_eq!(status.available_bytes, 900 - status.accounted_bytes);
    assert_eq!(status.repo_count, 1);
    assert_eq!(status.protected_repo_count, 1);
    assert_eq!(status.evictable_bytes, 0);

    drop(reservation);
}

fn write_repo_file(manager: &DiskManager, repo_path: &str, bytes: usize) {
    let repo_dir = manager.repos_dir().join(repo_path);
    fs::create_dir_all(&repo_dir).expect("repo dir");
    fs::write(repo_dir.join("objects.pack"), vec![0u8; bytes]).expect("repo file");
}

// ── Additional DiskManager correctness tests ─────────────────────

#[test]
fn new_and_status_returns_sensible_defaults() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    let status = manager.status().expect("status");

    assert_eq!(status.quota_bytes, 10_000);
    assert_eq!(status.reserved_bytes, 0);
    assert_eq!(status.repo_count, 0);
    assert_eq!(status.protected_repo_count, 0);
    assert_eq!(status.locked_repo_count, 0);
}

#[test]
fn reserve_succeeds_within_quota() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);

    let reservation = manager.reserve(100).expect("reserve");
    assert_eq!(reservation.bytes(), 100);

    let status = manager.status().expect("status");
    assert_eq!(status.reserved_bytes, 100);
}

#[test]
fn reserve_fails_when_exceeding_quota() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 50, 0);

    // Fill up with a repo file first
    write_repo_file(&manager, "big.git", 50);
    manager.record_repo_access("big.git").expect("access");
    manager
        .set_repo_protected("big.git", true)
        .expect("protect");

    let err = manager.reserve(10).expect_err("should be full");
    assert!(matches!(err, GitCacheError::DiskFull(_)));
}

#[test]
fn temp_path_is_under_root() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);

    let reservation = manager.reserve(64).expect("reserve");
    let temp = reservation.temp_path();
    assert!(temp.starts_with(root.path()));
    assert!(temp.to_str().unwrap().contains("tmp"));
}

#[test]
fn record_repo_access_creates_index_entry() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "test.git", 10);

    manager.record_repo_access("test.git").expect("access");

    let status = manager.status().expect("status");
    assert_eq!(status.repo_count, 1);
}

#[test]
fn record_repo_access_updates_existing_entry() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "test.git", 10);

    manager.record_repo_access("test.git").expect("first");
    std::thread::sleep(Duration::from_millis(2));
    manager.record_repo_access("test.git").expect("second");

    let status = manager.status().expect("status");
    assert_eq!(status.repo_count, 1);
}

#[test]
fn touch_repo_access_updates_existing_entry_without_resizing() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "test.git", 10);

    let first = manager.record_repo_access("test.git").expect("first");
    fs::write(
        manager.repos_dir().join("test.git").join("larger.pack"),
        vec![0u8; 100],
    )
    .expect("larger");
    std::thread::sleep(Duration::from_millis(2));
    let second = manager.touch_repo_access("test.git").expect("touch");

    assert_eq!(second.size_bytes, first.size_bytes);
    assert!(second.last_accessed_unix_millis > first.last_accessed_unix_millis);
}

#[test]
fn lock_repo_increments_locked_count() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "lock.git", 10);
    manager.record_repo_access("lock.git").expect("access");

    let lock = manager.lock_repo("lock.git").expect("lock");
    let status = manager.status().expect("status");
    assert_eq!(status.locked_repo_count, 1);

    drop(lock);
    let status = manager.status().expect("status after drop");
    assert_eq!(status.locked_repo_count, 0);
}

#[test]
fn set_repo_protected_marks_repo() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "prot.git", 10);
    manager.record_repo_access("prot.git").expect("access");

    manager
        .set_repo_protected("prot.git", true)
        .expect("protect");
    let status = manager.status().expect("status");
    assert_eq!(status.protected_repo_count, 1);

    manager
        .set_repo_protected("prot.git", false)
        .expect("unprotect");
    let status = manager.status().expect("status");
    assert_eq!(status.protected_repo_count, 0);
}

#[test]
fn invalidate_repo_removes_cache_and_index_metadata() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "stale.git", 10);
    manager.record_repo_access("stale.git").expect("access");
    manager
        .set_repo_protected("stale.git", true)
        .expect("protect");

    manager.invalidate_repo("stale.git").expect("invalidate");

    let index = manager.repo_index().expect("index");
    assert!(!index.repos.contains_key(Path::new("stale.git")));
    assert!(!manager.repos_dir().join("stale.git").exists());
    assert_eq!(manager.status().expect("status").protected_repo_count, 0);
}

#[test]
fn invalidate_repo_rejects_locked_repo() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "locked.git", 10);
    manager.record_repo_access("locked.git").expect("access");
    let _lock = manager.lock_repo("locked.git").expect("lock");

    let err = manager
        .invalidate_repo("locked.git")
        .expect_err("locked repo should not invalidate");

    assert!(matches!(err, GitCacheError::Conflict(_)));
    assert!(manager.repos_dir().join("locked.git").exists());
}

#[test]
fn invalidate_repo_leaves_repo_immediately_lockable() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "stale.git", 10);
    manager.record_repo_access("stale.git").expect("access");

    manager.invalidate_repo("stale.git").expect("invalidate");

    let _lock = manager
        .lock_repo("stale.git")
        .expect("repo must be lockable right after invalidation");
}

#[test]
fn pending_delete_temp_dirs_count_against_quota() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 100, 0);
    manager.ensure_layout().expect("layout");
    let trash = manager.temp_dir_for(Uuid::new_v4());
    fs::create_dir_all(&trash).expect("trash dir");
    fs::write(trash.join("pack"), vec![0u8; 80]).expect("trash payload");

    let status = manager.status().expect("status");
    assert!(status.used_bytes >= 80);
    let err = manager
        .reserve(50)
        .expect_err("reserve must account for pending-delete bytes");
    assert!(matches!(err, GitCacheError::DiskFull(_)));
}

#[test]
fn status_subtracts_min_free_from_quota_once() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 1_000, 100);

    let status = manager.status().expect("status");

    assert_eq!(status.quota_bytes, 1_000);
    assert_eq!(status.min_free_bytes, 100);
    assert_eq!(
        status.available_bytes,
        900_u64.saturating_sub(status.accounted_bytes)
    );
}

#[test]
fn lru_eviction_evicts_oldest_non_protected_non_locked() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 500, 0);

    write_repo_file(&manager, "oldest.git", 200);
    manager.record_repo_access("oldest.git").expect("oldest");
    std::thread::sleep(Duration::from_millis(2));

    write_repo_file(&manager, "middle.git", 200);
    manager.record_repo_access("middle.git").expect("middle");
    std::thread::sleep(Duration::from_millis(2));

    write_repo_file(&manager, "newest.git", 200);
    manager.record_repo_access("newest.git").expect("newest");

    // Need room → oldest should be evicted first
    let _reservation = manager.reserve(100).expect("reserve");

    assert!(!manager.repos_dir().join("oldest.git").exists());
    assert!(manager.repos_dir().join("newest.git").exists());
}

#[test]
fn multiple_locks_tracked_correctly() {
    let root = tempdir().expect("tempdir");
    let manager = DiskManager::new(root.path(), 10_000, 0);
    write_repo_file(&manager, "multi.git", 10);
    manager.record_repo_access("multi.git").expect("access");

    let lock1 = manager.lock_repo("multi.git").expect("lock1");
    let lock2 = manager.lock_repo("multi.git").expect("lock2");

    let status = manager.status().expect("status");
    assert_eq!(status.locked_repo_count, 1);

    drop(lock1);
    let status = manager.status().expect("status after drop1");
    assert_eq!(status.locked_repo_count, 1);

    drop(lock2);
    let status = manager.status().expect("status after drop2");
    assert_eq!(status.locked_repo_count, 0);
}
