//! Randomized concurrency fuzz for the DiskManager.
//!
//! Many OS threads hammer the full public surface (note/flush/record/touch
//! access, reserve/release, lock/unlock, invalidate, status, cleanup,
//! protect, index reads) with seeded random interleavings. The harness
//! checks that under arbitrary contention the manager never deadlocks
//! (watchdog), never poisons a mutex, never returns an unexpected error,
//! never over-allocates past the quota, and leaves a consistent index.

mod tests {
    use git_cache_core::GitCacheError;
    use git_cache_disk::DiskManager;
    use std::collections::HashSet;
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Deterministic per-thread PRNG (xorshift64*), no external deps.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed.max(1))
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn write_repo_file(manager: &DiskManager, repo_path: &str, bytes: usize) {
        let repo_dir = manager.repos_dir().join(repo_path);
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("objects.pack"), vec![1u8; bytes]).unwrap();
    }

    /// Like `write_repo_file`, but tolerates a concurrent `invalidate_repo`
    /// renaming the directory away between create and write — that interleaving
    /// is legal manager behavior, not a harness failure.
    fn write_repo_file_racy(manager: &DiskManager, repo_path: &str, bytes: usize) {
        let repo_dir = manager.repos_dir().join(repo_path);
        for _ in 0..8 {
            if std::fs::create_dir_all(&repo_dir).is_err() {
                continue;
            }
            if std::fs::write(repo_dir.join("objects.pack"), vec![1u8; bytes]).is_ok() {
                return;
            }
        }
    }

    fn repo_name(i: u64) -> String {
        format!("fuzz-{i}.git")
    }

    /// Errors that the public surface is allowed to return under contention.
    /// Anything else (in particular `Internal("... poisoned ...")`) is a bug.
    fn assert_allowed(err: &GitCacheError, op: &str) {
        match err {
            GitCacheError::Conflict(_)
            | GitCacheError::DiskFull(_)
            | GitCacheError::NotFound(_)
            | GitCacheError::Io(_) => {}
            other => panic!("unexpected error from {op}: {other}"),
        }
    }

    fn run_with_watchdog(name: &str, timeout: Duration, body: impl FnOnce() + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            body();
            let _ = tx.send(());
        });
        match rx.recv_timeout(timeout) {
            Ok(()) => handle.join().unwrap(),
            Err(_) => panic!("{name}: deadlock suspected — fuzz did not finish in {timeout:?}"),
        }
    }

    // ── 1. Full-surface random op fuzz ───────────────────────────────────────

    #[test]
    fn fuzz_full_surface_random_ops() {
        const THREADS: u64 = 16;
        const OPS_PER_THREAD: u64 = 400;
        const REPOS: u64 = 12;
        const QUOTA: u64 = 4 * 1024 * 1024;

        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(DiskManager::new(tmp.path(), QUOTA, 0));

        for i in 0..REPOS {
            write_repo_file(&manager, &repo_name(i), 4_000);
            manager.record_repo_access(repo_name(i)).unwrap();
        }

        let mgr = Arc::clone(&manager);
        run_with_watchdog(
            "fuzz_full_surface_random_ops",
            Duration::from_secs(120),
            move || {
                let handles: Vec<_> = (0..THREADS)
                    .map(|t| {
                        let mgr = Arc::clone(&mgr);
                        std::thread::spawn(move || {
                            let mut rng = Rng::new(0x9E37_79B9_7F4A_7C15 ^ (t + 1));
                            for _ in 0..OPS_PER_THREAD {
                                let repo = repo_name(rng.below(REPOS));
                                match rng.below(12) {
                                    0..=2 => {
                                        if let Err(e) = mgr.note_repo_access(&repo) {
                                            assert_allowed(&e, "note_repo_access");
                                        }
                                    }
                                    3 => {
                                        if let Err(e) = mgr.flush_repo_accesses() {
                                            assert_allowed(&e, "flush_repo_accesses");
                                        }
                                    }
                                    4 => {
                                        if let Err(e) = mgr.record_repo_access(&repo) {
                                            assert_allowed(&e, "record_repo_access");
                                        }
                                    }
                                    5 => {
                                        if let Err(e) = mgr.touch_repo_access(&repo) {
                                            assert_allowed(&e, "touch_repo_access");
                                        }
                                    }
                                    6 => {
                                        let bytes = 1 + rng.below(QUOTA / 8);
                                        match mgr.reserve(bytes) {
                                            Ok(res) => {
                                                if rng.below(2) == 0 {
                                                    std::fs::write(
                                                        res.temp_path().join("blob"),
                                                        vec![0u8; (bytes.min(16_384)) as usize],
                                                    )
                                                    .ok();
                                                }
                                                drop(res);
                                            }
                                            Err(e) => assert_allowed(&e, "reserve"),
                                        }
                                    }
                                    7 => match mgr.lock_repo(&repo) {
                                        Ok(lock) => {
                                            std::thread::yield_now();
                                            drop(lock);
                                        }
                                        Err(e) => assert_allowed(&e, "lock_repo"),
                                    },
                                    8 => {
                                        match mgr.invalidate_repo(&repo) {
                                            Ok(()) => {
                                                // Recreate so later ops have material to work on.
                                                write_repo_file_racy(&mgr, &repo, 4_000);
                                                if let Err(e) = mgr.record_repo_access(&repo) {
                                                    assert_allowed(&e, "record_repo_access");
                                                }
                                            }
                                            Err(e) => assert_allowed(&e, "invalidate_repo"),
                                        }
                                    }
                                    9 => match mgr.status() {
                                        Ok(status) => {
                                            assert_eq!(
                                                status.accounted_bytes,
                                                status.used_bytes + status.reserved_bytes,
                                                "accounted must equal used + reserved"
                                            );
                                            assert!(
                                                status.reserved_bytes <= status.quota_bytes,
                                                "reserved_bytes exceeded quota"
                                            );
                                        }
                                        Err(e) => assert_allowed(&e, "status"),
                                    },
                                    10 => {
                                        if let Err(e) =
                                            mgr.cleanup_stale_temps(Duration::from_secs(3600))
                                        {
                                            assert_allowed(&e, "cleanup_stale_temps");
                                        }
                                    }
                                    _ => {
                                        let protect = rng.below(2) == 0;
                                        if let Err(e) = mgr.set_repo_protected(&repo, protect) {
                                            assert_allowed(&e, "set_repo_protected");
                                        }
                                        if protect {
                                            // Leave nothing permanently unevictable.
                                            if let Err(e) = mgr.set_repo_protected(&repo, false) {
                                                assert_allowed(&e, "set_repo_protected");
                                            }
                                        }
                                    }
                                }
                            }
                        })
                    })
                    .collect();
                for handle in handles {
                    handle.join().unwrap();
                }
            },
        );

        // Post-conditions: nothing poisoned, index consistent, no leaks.
        manager.flush_repo_accesses().unwrap();
        let status = manager.status().unwrap();
        assert_eq!(status.reserved_bytes, 0, "all reservations released");
        assert_eq!(status.locked_repo_count, 0, "all repo locks released");
        let index = manager.repo_index().unwrap();
        for path in index.repos.keys() {
            assert!(
                manager.repos_dir().join(path).exists(),
                "index entry {} points at a missing directory",
                path.display()
            );
        }
    }

    // ── 2. note/flush durability under contention ────────────────────────────

    #[test]
    fn fuzz_note_flush_never_loses_existing_repo_access() {
        const THREADS: u64 = 8;
        const REPOS_PER_THREAD: u64 = 40;

        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(DiskManager::new(tmp.path(), 64 * 1024 * 1024, 0));

        let mgr = Arc::clone(&manager);
        run_with_watchdog(
            "fuzz_note_flush_never_loses_existing_repo_access",
            Duration::from_secs(120),
            move || {
                let mut handles = Vec::new();
                for t in 0..THREADS {
                    let mgr = Arc::clone(&mgr);
                    handles.push(std::thread::spawn(move || {
                        for i in 0..REPOS_PER_THREAD {
                            let repo = format!("note-{t}-{i}.git");
                            write_repo_file(&mgr, &repo, 64);
                            mgr.note_repo_access(&repo).unwrap();
                        }
                    }));
                }
                // Concurrent flusher racing the noters.
                let flusher = {
                    let mgr = Arc::clone(&mgr);
                    std::thread::spawn(move || {
                        for _ in 0..100 {
                            mgr.flush_repo_accesses().unwrap();
                            std::thread::yield_now();
                        }
                    })
                };
                for handle in handles {
                    handle.join().unwrap();
                }
                flusher.join().unwrap();
            },
        );

        manager.flush_repo_accesses().unwrap();
        let index = manager.repo_index().unwrap();
        let indexed: HashSet<_> = index.repos.keys().cloned().collect();
        for t in 0..THREADS {
            for i in 0..REPOS_PER_THREAD {
                let repo = format!("note-{t}-{i}.git");
                assert!(
                    indexed.contains(Path::new(&repo)),
                    "noted access for {repo} was lost"
                );
            }
        }
    }

    // ── 3. panic while holding guards must not wedge the manager ─────────────

    #[test]
    fn panicking_thread_holding_guards_does_not_wedge_manager() {
        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(DiskManager::new(tmp.path(), 64 * 1024 * 1024, 0));
        write_repo_file(&manager, "panicky.git", 100);
        manager.record_repo_access("panicky.git").unwrap();

        for _ in 0..50 {
            let mgr = Arc::clone(&manager);
            let result = std::thread::spawn(move || {
                let _lock = mgr.lock_repo("panicky.git").unwrap();
                let _reservation = mgr.reserve(1024).unwrap();
                panic!("injected panic while holding RepoLock + Reservation");
            })
            .join();
            assert!(result.is_err(), "thread must have panicked");
        }

        // Guards were dropped during unwind: nothing leaked, nothing poisoned.
        let status = manager.status().unwrap();
        assert_eq!(status.locked_repo_count, 0, "lock leaked across panic");
        assert_eq!(status.reserved_bytes, 0, "reservation leaked across panic");
        manager.note_repo_access("panicky.git").unwrap();
        manager.flush_repo_accesses().unwrap();
        manager.invalidate_repo("panicky.git").unwrap();
        let _relock = manager.lock_repo("panicky.git").unwrap();
    }

    // ── 4. invalidate vs lock vs serve race ──────────────────────────────────

    #[test]
    fn fuzz_invalidate_lock_race_is_conflict_or_clean_miss() {
        const ITERS: u64 = 300;

        let tmp = TempDir::new().unwrap();
        let manager = Arc::new(DiskManager::new(tmp.path(), 64 * 1024 * 1024, 0));
        write_repo_file(&manager, "hot.git", 1_000);
        manager.record_repo_access("hot.git").unwrap();

        let mgr = Arc::clone(&manager);
        run_with_watchdog(
            "fuzz_invalidate_lock_race_is_conflict_or_clean_miss",
            Duration::from_secs(120),
            move || {
                let locker = {
                    let mgr = Arc::clone(&mgr);
                    std::thread::spawn(move || {
                        let mut rng = Rng::new(7);
                        for _ in 0..ITERS {
                            match mgr.lock_repo("hot.git") {
                                Ok(lock) => {
                                    if rng.below(4) == 0 {
                                        std::thread::yield_now();
                                    }
                                    drop(lock);
                                }
                                Err(e) => assert_allowed(&e, "lock_repo"),
                            }
                        }
                    })
                };
                let invalidator = {
                    let mgr = Arc::clone(&mgr);
                    std::thread::spawn(move || {
                        for _ in 0..ITERS {
                            match mgr.invalidate_repo("hot.git") {
                                Ok(()) => {
                                    write_repo_file_racy(&mgr, "hot.git", 1_000);
                                    if let Err(e) = mgr.record_repo_access("hot.git") {
                                        assert_allowed(&e, "record_repo_access");
                                    }
                                }
                                Err(e) => assert_allowed(&e, "invalidate_repo"),
                            }
                        }
                    })
                };
                locker.join().unwrap();
                invalidator.join().unwrap();
            },
        );

        let status = manager.status().unwrap();
        assert_eq!(status.locked_repo_count, 0);
        // The repo must be in a coherent terminal state: either fully present
        // and indexed, or fully gone from both.
        let on_disk = manager.repos_dir().join("hot.git").exists();
        let indexed = manager
            .repo_index()
            .unwrap()
            .repos
            .contains_key(Path::new("hot.git"));
        assert_eq!(
            on_disk, indexed,
            "disk and index disagree about hot.git (disk={on_disk}, index={indexed})"
        );
    }
}
