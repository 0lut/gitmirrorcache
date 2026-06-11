//! Randomized concurrent fuzzing of the worker `UpdateCoordinator`.
//!
//! A chaos executor randomly delays, fails, or panics, while many tasks
//! submit randomized cron / read-through / event hints across a pool of
//! repos and branches. The coordinator must never deadlock, must always
//! release leases (even when the executor panics), and must keep its
//! in-flight dedup map clean so the system stays usable afterwards.

use async_trait::async_trait;
use git_cache_core::{BranchName, RepoKey, Result, Selector};
use git_cache_fuzz::FuzzConfig;
use git_cache_worker::{
    InMemoryRepoLeaseManager, UpdateCoordinator, UpdateDisposition, UpdateExecutor, UpdateRequest,
};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const REPO_POOL: usize = 6;
const BRANCH_POOL: [&str; 4] = ["main", "dev", "release", "hotfix"];

fn repo(i: usize) -> RepoKey {
    RepoKey::parse(format!("github.com/fuzz/repo-{i}")).expect("repo key")
}

fn branch(name: &str) -> Selector {
    Selector::Branch(BranchName::parse(name).expect("branch"))
}

/// Executor whose per-call behavior is derived from a seeded counter:
/// random small delays, intermittent errors, and rare panics.
struct ChaosExecutor {
    seed: u64,
    calls: AtomicU64,
    panics: AtomicUsize,
    errors: AtomicUsize,
    successes: AtomicUsize,
}

impl ChaosExecutor {
    fn new(seed: u64) -> Self {
        Self {
            seed,
            calls: AtomicU64::new(0),
            panics: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
            successes: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl UpdateExecutor for ChaosExecutor {
    async fn update(&self, _request: UpdateRequest) -> Result<()> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let mut rng = fastrand::Rng::with_seed(self.seed ^ call.wrapping_mul(0x9e3779b97f4a7c15));

        tokio::time::sleep(Duration::from_micros(rng.u64(..2_000))).await;

        match rng.u32(..100) {
            // Rare panic: the coordinator must convert this into an error
            // and still release the repo lease.
            0..=2 => {
                self.panics.fetch_add(1, Ordering::SeqCst);
                panic!("chaos executor injected panic");
            }
            3..=19 => {
                self.errors.fetch_add(1, Ordering::SeqCst);
                Err(git_cache_core::GitCacheError::UpstreamUnavailable(
                    "chaos executor injected failure".into(),
                ))
            }
            _ => {
                self.successes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn coordinator_survives_randomized_chaos() {
    let config = FuzzConfig::from_env("coordinator_fuzz", 16, 80);
    let executor = Arc::new(ChaosExecutor::new(config.seed));
    let coordinator = UpdateCoordinator::new(
        Arc::clone(&executor) as Arc<dyn UpdateExecutor>,
        Arc::new(InMemoryRepoLeaseManager::new()),
    );

    let run = async {
        let mut handles = Vec::new();
        for task in 0..config.tasks {
            let mut rng = config.task_rng(task);
            let coordinator = coordinator.clone();
            let ops = config.ops_per_task;

            handles.push(tokio::spawn(async move {
                let mut outcomes = [0usize; 3]; // updated, lease_busy, errors
                for _ in 0..ops {
                    let repo = repo(rng.usize(..REPO_POOL));
                    let selector = branch(BRANCH_POOL[rng.usize(..BRANCH_POOL.len())]);
                    let result = match rng.u32(..100) {
                        0..=44 => coordinator.read_through(repo, selector).await,
                        45..=74 => coordinator.cron(repo, selector).await,
                        75..=94 => {
                            let branch_name = BRANCH_POOL[rng.usize(..BRANCH_POOL.len())];
                            coordinator
                                .event_hint(repo, format!("refs/heads/{branch_name}"))
                                .await
                        }
                        // Invalid event refs must be rejected cleanly.
                        _ => {
                            let result = coordinator.event_hint(repo, "refs/heads/../escape").await;
                            assert!(result.is_err(), "invalid event ref must be rejected");
                            continue;
                        }
                    };

                    match result {
                        Ok(outcome) => match outcome.disposition {
                            UpdateDisposition::Updated => outcomes[0] += 1,
                            UpdateDisposition::LeaseBusy => outcomes[1] += 1,
                        },
                        Err(_) => outcomes[2] += 1,
                    }

                    if rng.u32(..8) == 0 {
                        tokio::task::yield_now().await;
                    }
                }
                outcomes
            }));
        }

        let mut totals = [0usize; 3];
        for handle in handles {
            let outcomes = handle.await.expect("fuzz task must not panic");
            for (total, value) in totals.iter_mut().zip(outcomes) {
                *total += value;
            }
        }
        totals
    };

    let totals = tokio::time::timeout(config.deadline, run)
        .await
        .expect("coordinator fuzz deadlocked: requests did not finish before the deadline");

    eprintln!(
        "[coordinator_fuzz] updated={} lease_busy={} errors={} executor: calls={} ok={} err={} panics={}",
        totals[0],
        totals[1],
        totals[2],
        executor.calls.load(Ordering::SeqCst),
        executor.successes.load(Ordering::SeqCst),
        executor.errors.load(Ordering::SeqCst),
        executor.panics.load(Ordering::SeqCst),
    );
    assert!(totals[0] > 0, "at least some updates should succeed");

    // After the storm every repo must be immediately serviceable: no leaked
    // leases (a leaked lease would yield LeaseBusy forever) and no stale
    // in-flight entries (which would replay old results).
    for i in 0..REPO_POOL {
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match coordinator.read_through(repo(i), branch("main")).await {
                    Ok(outcome) if outcome.disposition == UpdateDisposition::Updated => {
                        return outcome
                    }
                    // Chaos executor may still inject errors; retry until the
                    // success path proves the lease is free.
                    _ => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("repo {i}: lease or inflight entry leaked after fuzz"));
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn same_key_burst_deduplicates_and_completes() {
    let config = FuzzConfig::from_env("coordinator_burst_fuzz", 32, 20);
    let executor = Arc::new(ChaosExecutor::new(config.seed));
    let coordinator = UpdateCoordinator::new(
        Arc::clone(&executor) as Arc<dyn UpdateExecutor>,
        Arc::new(InMemoryRepoLeaseManager::new()),
    );

    let run = async {
        // Repeated bursts where every task hits the SAME key simultaneously,
        // maximizing contention on the inflight map and the watch channel.
        for round in 0..config.ops_per_task {
            let barrier = Arc::new(tokio::sync::Barrier::new(config.tasks));
            let mut handles = Vec::new();
            for _ in 0..config.tasks {
                let coordinator = coordinator.clone();
                let barrier = Arc::clone(&barrier);
                let key_repo = repo(round % REPO_POOL);
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    coordinator.read_through(key_repo, branch("main")).await
                }));
            }
            for handle in handles {
                // Joining tasks may observe an injected error; they must
                // never hang or panic.
                let _ = handle.await.expect("burst task must not panic");
            }
        }
    };

    tokio::time::timeout(config.deadline, run)
        .await
        .expect("same-key burst fuzz deadlocked");
}
