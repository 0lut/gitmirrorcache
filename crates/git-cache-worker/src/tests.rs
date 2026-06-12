use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::time::{advance, timeout};

#[derive(Default)]
struct RecordingExecutor {
    calls: AtomicUsize,
    requests: Mutex<Vec<UpdateRequest>>,
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

    async fn requests(&self) -> Vec<UpdateRequest> {
        self.requests.lock().await.clone()
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
    async fn update(&self, request: UpdateRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().await.push(request);
        self.started.notify_waiters();

        if self.hold {
            self.release.notified().await;
        }

        Ok(())
    }
}

fn repo() -> RepoKey {
    RepoKey::parse("github.com/acme/project").unwrap()
}

fn branch(name: &str) -> Selector {
    Selector::Branch(BranchName::parse(name).unwrap())
}

fn coordinator(executor: Arc<RecordingExecutor>) -> UpdateCoordinator {
    UpdateCoordinator::new(executor, Arc::new(InMemoryRepoLeaseManager::new()))
}

// ── UpdateTarget::from_selector tests ─────────────────────────

#[test]
fn from_selector_branch() {
    let selector = branch("main");
    let target = UpdateTarget::from_selector(&selector);
    assert_eq!(
        target,
        UpdateTarget::Branch(BranchName::parse("main").unwrap())
    );
}

#[test]
fn from_selector_default_branch() {
    let target = UpdateTarget::from_selector(&Selector::DefaultBranch);
    assert_eq!(target, UpdateTarget::DefaultBranch);
}

#[test]
fn from_selector_commit() {
    let commit = CommitSha::parse("a".repeat(40)).unwrap();
    let target = UpdateTarget::from_selector(&Selector::Commit(commit.clone()));
    assert_eq!(target, UpdateTarget::Commit(commit));
}

#[test]
fn from_selector_short_commit() {
    let short = ShortCommitSha::parse("abcdef12").unwrap();
    let target = UpdateTarget::from_selector(&Selector::ShortCommit(short.clone()));
    assert_eq!(target, UpdateTarget::ShortCommit(short));
}

// ── UpdateTarget::from_event_ref tests ──────────────────────────

#[test]
fn from_event_ref_parses_branch() {
    let target = UpdateTarget::from_event_ref("refs/heads/main").unwrap();
    assert_eq!(
        target,
        UpdateTarget::Branch(BranchName::parse("main").unwrap())
    );
}

#[test]
fn from_event_ref_parses_tag_as_ref() {
    let target = UpdateTarget::from_event_ref("refs/tags/v1").unwrap();
    assert_eq!(target, UpdateTarget::Ref("refs/tags/v1".to_string()));
}

// ── validate_event_ref tests ────────────────────────────────────

#[test]
fn validate_event_ref_rejects_empty() {
    assert!(validate_event_ref("").is_err());
}

#[test]
fn validate_event_ref_rejects_control_chars() {
    assert!(validate_event_ref("refs/heads/\x01bad").is_err());
}

#[test]
fn validate_event_ref_rejects_backslash() {
    assert!(validate_event_ref("refs\\heads\\main").is_err());
}

#[test]
fn validate_event_ref_rejects_dot_dot() {
    assert!(validate_event_ref("refs/heads/../main").is_err());
}

#[test]
fn validate_event_ref_rejects_ending_lock() {
    assert!(validate_event_ref("refs/heads/main.lock").is_err());
}

// ── UpdateHint::into_request tests ──────────────────────────────

#[test]
fn cron_hint_into_request() {
    let hint = UpdateHint::Cron {
        repo: repo(),
        selector: branch("main"),
    };
    let request = hint.into_request().unwrap();
    assert_eq!(request.source, UpdateSource::Cron);
    assert_eq!(
        request.target,
        UpdateTarget::Branch(BranchName::parse("main").unwrap())
    );
}

#[test]
fn read_through_hint_into_request() {
    let hint = UpdateHint::ReadThrough {
        repo: repo(),
        selector: Selector::DefaultBranch,
    };
    let request = hint.into_request().unwrap();
    assert_eq!(request.source, UpdateSource::ReadThrough);
    assert_eq!(request.target, UpdateTarget::DefaultBranch);
}

#[test]
fn event_hint_into_request() {
    let hint = UpdateHint::Event {
        repo: repo(),
        ref_name: "refs/heads/main".to_string(),
    };
    let request = hint.into_request().unwrap();
    assert_eq!(request.source, UpdateSource::Event);
    assert_eq!(
        request.target,
        UpdateTarget::Branch(BranchName::parse("main").unwrap())
    );
}

// ── CronUpdateConfig::try_new tests ─────────────────────────────

#[test]
fn cron_config_rejects_zero_interval() {
    let result = CronUpdateConfig::try_new(Duration::ZERO, vec![]);
    assert!(result.is_err());
}

#[test]
fn cron_config_accepts_nonzero_interval() {
    let config = CronUpdateConfig::try_new(Duration::from_secs(10), vec![]).unwrap();
    assert_eq!(config.interval, Duration::from_secs(10));
    assert!(!config.run_immediately);
}

// ── EventHint::new tests ────────────────────────────────────────

#[test]
fn event_hint_new_accepts_valid_ref() {
    let hint = EventHint::new(repo(), "refs/heads/main").unwrap();
    assert_eq!(hint.ref_name, "refs/heads/main");
}

#[test]
fn event_hint_new_rejects_invalid_ref() {
    assert!(EventHint::new(repo(), "").is_err());
    assert!(EventHint::new(repo(), "refs/heads/../main").is_err());
}

// ── StopHandle/StopSignal tests ─────────────────────────────────

#[tokio::test]
async fn stop_handle_triggers_stop_signal() {
    let (handle, mut signal) = stop_channel();
    handle.stop();
    // cancelled() should return immediately after stop
    tokio::time::timeout(Duration::from_millis(100), signal.cancelled())
        .await
        .expect("cancelled should resolve after stop");
}

#[tokio::test]
async fn dedupes_concurrent_updates_for_same_repo_and_ref() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coordinator = coordinator(Arc::clone(&executor));
    let read_through = ReadThroughUpdatePath::new(coordinator);

    let first = tokio::spawn({
        let read_through = read_through.clone();
        async move { read_through.update(repo(), branch("main")).await }
    });
    executor.wait_started().await;

    let second = tokio::spawn({
        let read_through = read_through.clone();
        async move { read_through.update(repo(), branch("main")).await }
    });

    tokio::task::yield_now().await;
    assert_eq!(executor.calls(), 1);

    executor.release_one();
    let first = first.await.unwrap().unwrap();
    let second = second.await.unwrap().unwrap();

    assert_eq!(executor.calls(), 1);
    assert_eq!(first.disposition, UpdateDisposition::Updated);
    assert_eq!(second.disposition, UpdateDisposition::Updated);
    assert_eq!(first.key, second.key);
}

#[tokio::test]
async fn per_repo_lease_excludes_concurrent_updates_for_different_refs() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coordinator = coordinator(Arc::clone(&executor));

    let first = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { coordinator.read_through(repo(), branch("main")).await }
    });
    executor.wait_started().await;

    let busy = coordinator
        .read_through(repo(), branch("release"))
        .await
        .unwrap();

    assert_eq!(busy.disposition, UpdateDisposition::LeaseBusy);
    assert_eq!(executor.calls(), 1);

    executor.release_one();
    assert_eq!(
        first.await.unwrap().unwrap().disposition,
        UpdateDisposition::Updated
    );
}

#[tokio::test]
async fn event_hint_intake_processes_submitted_hints() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coordinator = coordinator(Arc::clone(&executor));
    let (sender, receiver) = event_hint_channel(4);
    let mut intake = EventHintIntake::new(receiver, coordinator);

    sender
        .submit(EventHint::new(repo(), "refs/heads/main").unwrap())
        .await
        .unwrap();

    let outcome = intake.drain_once().await.unwrap().unwrap();
    let requests = executor.requests().await;

    assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    assert_eq!(outcome.source, UpdateSource::Event);
    assert_eq!(executor.calls(), 1);
    assert_eq!(requests[0].source, UpdateSource::Event);
    assert_eq!(
        requests[0].target,
        UpdateTarget::Branch(BranchName::parse("main").unwrap())
    );
}

#[tokio::test(start_paused = true)]
async fn cron_loop_ticks_on_configured_interval_and_stops() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coordinator = coordinator(Arc::clone(&executor));
    let config = CronUpdateConfig::try_new(
        Duration::from_secs(10),
        vec![CronJob::new(repo(), branch("main"))],
    )
    .unwrap();
    let cron = CronUpdateLoop::new(coordinator, config);
    let (stop, stop_signal) = stop_channel();

    let task = tokio::spawn(cron.run_until(stop_signal));
    tokio::task::yield_now().await;
    assert_eq!(executor.calls(), 0);

    advance(Duration::from_secs(9)).await;
    tokio::task::yield_now().await;
    assert_eq!(executor.calls(), 0);

    advance(Duration::from_secs(1)).await;
    timeout(Duration::from_secs(1), async {
        while executor.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(executor.calls(), 1);

    stop.stop();
    timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

// ── Performance tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_high_volume_deduplication() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coordinator = coordinator(Arc::clone(&executor));
    let concurrent_calls = 100;

    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for _ in 0..concurrent_calls {
        let coordinator = coordinator.clone();
        handles.push(tokio::spawn(async move {
            coordinator.read_through(repo(), branch("main")).await
        }));
    }

    // Wait for the executor to start, then release.
    executor.wait_started().await;
    tokio::task::yield_now().await;
    executor.release_one();

    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.unwrap().unwrap());
    }
    let elapsed = start.elapsed();

    assert_eq!(
        executor.calls(),
        1,
        "expected exactly 1 executor call for {concurrent_calls} concurrent requests, got {}",
        executor.calls()
    );

    for outcome in &outcomes {
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    }

    eprintln!("high-volume dedup: {concurrent_calls} callers, 1 executor call, {elapsed:?}");
}

#[tokio::test]
async fn test_mixed_key_throughput() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coordinator = coordinator(Arc::clone(&executor));
    let keys_count = 10;
    let requests_per_key = 5;

    let start = std::time::Instant::now();
    let mut handles = Vec::new();
    for k in 0..keys_count {
        let branch_name = format!("branch-{k}");
        for _ in 0..requests_per_key {
            let coordinator = coordinator.clone();
            let branch_name = branch_name.clone();
            handles.push(tokio::spawn(async move {
                coordinator.read_through(repo(), branch(&branch_name)).await
            }));
        }
    }

    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.unwrap().unwrap());
    }
    let elapsed = start.elapsed();

    // With no hold, each key should be processed. Some dedup may occur.
    let calls = executor.calls();
    assert!(
        calls >= keys_count && calls <= keys_count * requests_per_key,
        "expected between {keys_count} and {} calls, got {calls}",
        keys_count * requests_per_key
    );

    eprintln!(
        "mixed-key throughput: {} requests across {keys_count} keys, {calls} executor calls, {elapsed:?}",
        keys_count * requests_per_key
    );
}

#[tokio::test]
async fn test_event_hint_channel_throughput() {
    let (sender, mut receiver) = event_hint_channel(1024);
    let hint_count = 1000;

    let start = std::time::Instant::now();
    for i in 0..hint_count {
        let ref_name = format!("refs/heads/branch-{i}");
        sender
            .submit(EventHint::new(repo(), &ref_name).unwrap())
            .await
            .unwrap();
    }
    let send_elapsed = start.elapsed();

    let start = std::time::Instant::now();
    let mut received = 0;
    // Drop sender so receiver knows when channel is closed.
    drop(sender);
    while receiver.next_hint().await.unwrap().is_some() {
        received += 1;
    }
    let recv_elapsed = start.elapsed();

    assert_eq!(
        received, hint_count,
        "expected {hint_count} hints, got {received}"
    );

    let total = send_elapsed + recv_elapsed;
    let throughput = hint_count as f64 / total.as_secs_f64();
    eprintln!(
        "event hint channel: send={send_elapsed:?}, recv={recv_elapsed:?}, total={total:?} ({throughput:.0} hints/sec)"
    );
}

#[tokio::test(start_paused = true)]
async fn test_cron_tick_latency() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coordinator = coordinator(Arc::clone(&executor));
    let job_count = 10;
    let jobs: Vec<CronJob> = (0..job_count)
        .map(|i| CronJob::new(repo(), branch(&format!("branch-{i}"))))
        .collect();
    let config = CronUpdateConfig::try_new(Duration::from_secs(60), jobs).unwrap();
    let cron = CronUpdateLoop::new(coordinator, config);

    let start = std::time::Instant::now();
    let outcomes = cron.tick_once().await.unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        outcomes.len(),
        job_count,
        "expected {job_count} outcomes, got {}",
        outcomes.len()
    );
    for outcome in &outcomes {
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    }
    assert_eq!(executor.calls(), job_count);

    eprintln!("cron tick_once: {job_count} jobs completed in {elapsed:?}");
}

// ── Contention Tests ─────────────────────────────────────────────────

#[derive(Default)]
struct PanicExecutor;

#[async_trait]
impl UpdateExecutor for PanicExecutor {
    async fn update(&self, _request: UpdateRequest) -> Result<()> {
        panic!("executor panicked intentionally");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inflight_dedup_thundering_herd() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coord = coordinator(Arc::clone(&executor));
    let key = UpdateKey::new(repo(), UpdateTarget::from_selector(&branch("main")));

    let first = tokio::spawn({
        let c = coord.clone();
        async move { c.read_through(repo(), branch("main")).await }
    });
    executor.wait_started().await;

    let mut joiners = Vec::new();
    for _ in 0..99 {
        let c = coord.clone();
        joiners.push(tokio::spawn(async move {
            c.read_through(repo(), branch("main")).await
        }));
    }

    timeout(Duration::from_secs(5), async {
        while coord.inflight_waiter_count(&key).await < joiners.len() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("all joiners should attach to the in-flight update before release");
    assert_eq!(
        coord.inflight_waiter_count(&key).await,
        joiners.len(),
        "all joiners should attach to the in-flight update before release"
    );
    assert_eq!(executor.calls(), 1, "executor called exactly once");

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

#[tokio::test(flavor = "multi_thread")]
async fn mixed_inflight_and_new_requests() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coord = coordinator(Arc::clone(&executor));

    let main_task = tokio::spawn({
        let c = coord.clone();
        async move { c.read_through(repo(), branch("main")).await }
    });
    executor.wait_started().await;

    // Same repo+branch should join inflight.
    let join_task = tokio::spawn({
        let c = coord.clone();
        async move { c.read_through(repo(), branch("main")).await }
    });

    // Same repo, different branch should get LeaseBusy.
    let busy_result = coord.read_through(repo(), branch("release")).await.unwrap();
    assert_eq!(busy_result.disposition, UpdateDisposition::LeaseBusy);

    executor.release_one();
    let main_result = main_task.await.unwrap().unwrap();
    let join_result = join_task.await.unwrap().unwrap();

    assert_eq!(main_result.disposition, UpdateDisposition::Updated);
    assert_eq!(join_result.disposition, UpdateDisposition::Updated);
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_fire_different_repos() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coord = UpdateCoordinator::new(
        executor as Arc<dyn UpdateExecutor>,
        Arc::new(InMemoryRepoLeaseManager::new()),
    );

    let handles: Vec<_> = (0..50)
        .map(|i| {
            let c = coord.clone();
            let repo = RepoKey::parse(format!("github.com/org/repo-{i}")).unwrap();
            tokio::spawn(async move { c.read_through(repo, branch("main")).await })
        })
        .collect();

    for handle in handles {
        let outcome = handle.await.unwrap().unwrap();
        assert_eq!(outcome.disposition, UpdateDisposition::Updated);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lease_release_and_immediate_reacquire() {
    let executor = Arc::new(RecordingExecutor::new(false));
    let coord = coordinator(Arc::clone(&executor));

    let first = coord.read_through(repo(), branch("main")).await.unwrap();
    assert_eq!(first.disposition, UpdateDisposition::Updated);
    assert_eq!(executor.calls(), 1);

    let second = coord.read_through(repo(), branch("main")).await.unwrap();
    assert_eq!(second.disposition, UpdateDisposition::Updated);
    assert_eq!(executor.calls(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn executor_panic_does_not_deadlock_waiters() {
    let executor = Arc::new(PanicExecutor);
    let coord = UpdateCoordinator::new(
        executor as Arc<dyn UpdateExecutor>,
        Arc::new(InMemoryRepoLeaseManager::new()),
    );

    let handle1 = tokio::spawn({
        let c = coord.clone();
        async move { c.read_through(repo(), branch("main")).await }
    });

    let handle2 = tokio::spawn({
        let c = coord.clone();
        async move { c.read_through(repo(), branch("main")).await }
    });

    let timeout_result = timeout(Duration::from_secs(5), async {
        let r1 = handle1.await;
        let r2 = handle2.await;
        (r1, r2)
    })
    .await;

    // The test passes if we don't deadlock (timeout doesn't fire).
    // At least one will see a panic or error propagated.
    assert!(
        timeout_result.is_ok(),
        "coordinator must not deadlock on executor panic"
    );
}

#[tokio::test]
async fn event_hint_channel_backpressure() {
    let (sender, mut receiver) = event_hint_channel(2);

    sender
        .submit(EventHint::new(repo(), "refs/heads/main").unwrap())
        .await
        .unwrap();
    sender
        .submit(EventHint::new(repo(), "refs/heads/dev").unwrap())
        .await
        .unwrap();

    // Channel is full (capacity 2). A third send should not complete
    // immediately (it blocks). We verify by trying with a timeout.
    let send_task = tokio::spawn({
        let sender = sender.clone();
        async move {
            sender
                .submit(EventHint::new(repo(), "refs/heads/feat").unwrap())
                .await
        }
    });

    // Give it a moment — it should be blocked.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !send_task.is_finished(),
        "send should be blocked by backpressure"
    );

    // Drain one message.
    let _ = receiver.next_hint().await.unwrap();

    // Now the blocked sender should complete.
    let result = timeout(Duration::from_secs(2), send_task).await;
    assert!(result.is_ok(), "send should complete after drain");
    result.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cron_and_read_through_contention() {
    let executor = Arc::new(RecordingExecutor::new(true));
    let coord = coordinator(Arc::clone(&executor));
    let cron_config = CronUpdateConfig::try_new(
        Duration::from_millis(1),
        vec![CronJob::new(repo(), branch("main"))],
    )
    .unwrap()
    .with_run_immediately(true);

    let cron_loop = CronUpdateLoop::new(coord.clone(), cron_config);

    // Start cron in background.
    let (stop, stop_signal) = stop_channel();
    let cron_task = tokio::spawn(cron_loop.run_until(stop_signal));

    // Wait for cron to start its first update.
    executor.wait_started().await;

    // Attempt a read_through for the same repo — should get LeaseBusy
    // because cron holds the repo lease.
    let read_result = coord.read_through(repo(), branch("release")).await.unwrap();
    assert_eq!(read_result.disposition, UpdateDisposition::LeaseBusy);

    executor.release_one();
    stop.stop();
    let _ = timeout(Duration::from_secs(2), cron_task).await;
}
