//! Worker contention tests.
//!
//! Tests cover concurrent read_through for same key (deduplication), concurrent
//! read_through for different keys, lease busy handling, and queue overflow behavior.

use async_trait::async_trait;
use git_cache_core::{BranchName, RepoKey, Result, Selector};
use git_cache_worker::{
    InMemoryRepoLeaseManager, LeaseAcquire, ReadThroughUpdatePath, RepoLeaseManager,
    UpdateCoordinator, UpdateDisposition, UpdateExecutor, UpdateRequest,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Barrier, Notify};

#[derive(Default)]
struct RecordingExecutor {
    calls: AtomicUsize,
    hold: bool,
    started: Notify,
    release: Notify,
}

impl RecordingExecutor {
    fn new(hold: bool) -> Self {
        Self {
            hold,
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn wait_started(&self) {
        let notified = self.started.notified();
        if self.calls() == 0 {
            notified.await;
        }
    }

    fn release_one(&self) {
        self.release.notify_one();
    }
}

#[async_trait]
impl UpdateExecutor for RecordingExecutor {
    async fn update(&self, _request: UpdateRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_waiters();
        if self.hold {
            self.release.notified().await;
        }
        Ok(())
    }
}

/// An executor that delays for a configurable duration.
struct SlowExecutor {
    calls: AtomicUsize,
    delay: Duration,
}

impl SlowExecutor {
    fn new(delay: Duration) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            delay,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl UpdateExecutor for SlowExecutor {
    async fn update(&self, _request: UpdateRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(())
    }
}

/// A lease manager that tracks acquire/release counts.
struct CountingLeaseManager {
    inner: InMemoryRepoLeaseManager,
    acquires: AtomicUsize,
    busy_count: AtomicUsize,
}

impl CountingLeaseManager {
    fn new() -> Self {
        Self {
            inner: InMemoryRepoLeaseManager::new(),
            acquires: AtomicUsize::new(0),
            busy_count: AtomicUsize::new(0),
        }
    }

    fn acquires(&self) -> usize {
        self.acquires.load(Ordering::SeqCst)
    }

    fn busy_count(&self) -> usize {
        self.busy_count.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl RepoLeaseManager for CountingLeaseManager {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire> {
        let result = self.inner.acquire(repo).await?;
        match &result {
            LeaseAcquire::Acquired(_) => {
                self.acquires.fetch_add(1, Ordering::SeqCst);
            }
            LeaseAcquire::Busy => {
                self.busy_count.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(result)
    }
}

fn repo() -> RepoKey {
    RepoKey::parse("github.com/acme/project").unwrap()
}

fn repo_n(n: usize) -> RepoKey {
    RepoKey::parse(format!("github.com/acme/project-{n}")).unwrap()
}

fn branch(name: &str) -> Selector {
    Selector::Branch(BranchName::parse(name).unwrap())
}

fn coordinator(executor: Arc<dyn UpdateExecutor>) -> UpdateCoordinator {
    UpdateCoordinator::new(executor, Arc::new(InMemoryRepoLeaseManager::new()))
}

fn coordinator_with_leases(
    executor: Arc<dyn UpdateExecutor>,
    leases: Arc<dyn RepoLeaseManager>,
) -> UpdateCoordinator {
    UpdateCoordinator::new(executor, leases)
}

// ── 1. Concurrent read_through for same key (deduplication) ─────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_read_through_same_key_deduplicates() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coord = coordinator(Arc::clone(&executor) as Arc<dyn UpdateExecutor>);
    let read_through = ReadThroughUpdatePath::new(coord);

    // First task starts the update.
    let first = {
        let rt = read_through.clone();
        tokio::spawn(async move { rt.update(repo(), branch("main")).await })
    };
    executor.wait_started().await;

    // 9 more tasks join the inflight update.
    let mut joiners = Vec::new();
    for _ in 0..9 {
        let rt = read_through.clone();
        joiners.push(tokio::spawn(async move {
            rt.update(repo(), branch("main")).await
        }));
    }

    // Only one actual execution should happen.
    tokio::task::yield_now().await;
    assert_eq!(
        executor.calls(),
        1,
        "only one actual update should execute for 10 concurrent requests"
    );

    // Release the single execution.
    executor.release_one();

    let first_result = first.await.unwrap().unwrap();
    assert_eq!(first_result.disposition, UpdateDisposition::Updated);

    for handle in joiners {
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
        assert_eq!(outcome.key, first_result.key);
    }

    assert_eq!(executor.calls(), 1, "still exactly one executor call");
}

// ── 2. Concurrent read_through for different keys ───────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_read_through_different_keys_proceed_independently() {
    let executor = Arc::new(SlowExecutor::new(Duration::from_millis(10)));
    let coord = coordinator(Arc::clone(&executor) as Arc<dyn UpdateExecutor>);
    let barrier = Arc::new(Barrier::new(15));

    // 5 different repos, 3 tasks each.
    let mut handles = Vec::new();
    for repo_idx in 0..5 {
        for _ in 0..3 {
            let c = coord.clone();
            let bar = Arc::clone(&barrier);
            let r = repo_n(repo_idx);
            handles.push(tokio::spawn(async move {
                bar.wait().await;
                c.read_through(r, branch("main")).await
            }));
        }
    }

    let results: Vec<_> = futures::future::join_all(handles)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();

    // All should get Updated (different repos = different leases).
    for outcome in &results {
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    }

    // The executor should be called at least 5 times (once per repo at minimum,
    // possibly more if dedup didn't collapse all 3 requests per repo).
    let total_calls = executor.calls();
    assert!(
        total_calls >= 5,
        "at least 5 executor calls for 5 repos, got {total_calls}"
    );
    assert!(
        total_calls <= 15,
        "at most 15 executor calls, got {total_calls}"
    );
}

// ── 3. Lease busy handling ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn lease_busy_while_update_in_progress() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let leases = Arc::new(CountingLeaseManager::new());
    let coord = coordinator_with_leases(
        Arc::clone(&executor) as Arc<dyn UpdateExecutor>,
        Arc::clone(&leases) as Arc<dyn RepoLeaseManager>,
    );

    // Start an update for "main" branch — this holds the repo lease.
    let first = {
        let c = coord.clone();
        tokio::spawn(async move { c.read_through(repo(), branch("main")).await })
    };
    executor.wait_started().await;

    // Requests for a DIFFERENT branch on the SAME repo should get LeaseBusy.
    let mut busy_handles = Vec::new();
    for i in 0..5 {
        let c = coord.clone();
        busy_handles.push(tokio::spawn(async move {
            c.read_through(repo(), branch(&format!("feature-{i}")))
                .await
        }));
    }

    let busy_results: Vec<_> = futures::future::join_all(busy_handles)
        .await
        .into_iter()
        .map(|r| r.unwrap().unwrap())
        .collect();

    for outcome in &busy_results {
        assert_eq!(
            outcome.disposition,
            UpdateDisposition::LeaseBusy,
            "requests for different branches on same repo should get LeaseBusy"
        );
    }

    // Release the first update.
    executor.release_one();
    let first_result = first.await.unwrap().unwrap();
    assert_eq!(first_result.disposition, UpdateDisposition::Updated);

    // Verify lease was acquired exactly once.
    assert_eq!(leases.acquires(), 1);
    assert!(leases.busy_count() >= 5);
}

// ── 4. Queue overflow behavior ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn queue_overflow_no_oom_or_deadlock() {
    let executor = Arc::new(SlowExecutor::new(Duration::from_millis(1)));
    let coord = coordinator(Arc::clone(&executor) as Arc<dyn UpdateExecutor>);

    // Submit many updates rapidly across different repos.
    let mut handles = Vec::new();
    for i in 0..200 {
        let c = coord.clone();
        let r = repo_n(i % 50); // 50 unique repos, 4 requests each.
        handles.push(tokio::spawn(async move {
            c.read_through(r, branch("main")).await
        }));
    }

    // Use a timeout to detect deadlocks.
    let all_results =
        tokio::time::timeout(Duration::from_secs(30), futures::future::join_all(handles)).await;

    assert!(
        all_results.is_ok(),
        "system should not deadlock under queue overflow"
    );

    let results = all_results.unwrap();
    let mut updated = 0;
    let mut lease_busy = 0;
    let mut errors = 0;

    for result in results {
        match result.unwrap() {
            Ok(outcome) => match outcome.disposition {
                UpdateDisposition::Updated => updated += 1,
                UpdateDisposition::LeaseBusy => lease_busy += 1,
            },
            Err(_) => errors += 1,
        }
    }

    assert!(updated > 0, "at least some updates should succeed");

    eprintln!(
        "queue overflow: updated={updated}, lease_busy={lease_busy}, errors={errors}, executor_calls={}",
        executor.calls()
    );

    // Verify the system is still functional after the flood.
    let post_flood = coord
        .read_through(repo_n(999), branch("main"))
        .await
        .unwrap();
    assert_eq!(post_flood.disposition, UpdateDisposition::Updated);
}
