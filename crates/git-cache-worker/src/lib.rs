use async_trait::async_trait;
use git_cache_core::{
    BranchName, CommitSha, GitCacheError, RepoKey, Result, Selector, ShortCommitSha,
};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    Cron,
    ReadThrough,
    Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateTarget {
    Branch(BranchName),
    DefaultBranch,
    Commit(CommitSha),
    ShortCommit(ShortCommitSha),
    Ref(String),
}

impl UpdateTarget {
    pub fn from_selector(selector: &Selector) -> Self {
        match selector {
            Selector::Branch(branch) => Self::Branch(branch.clone()),
            Selector::DefaultBranch => Self::DefaultBranch,
            Selector::Commit(commit) => Self::Commit(commit.clone()),
            Selector::ShortCommit(commit) => Self::ShortCommit(commit.clone()),
        }
    }

    pub fn from_event_ref(ref_name: impl Into<String>) -> Result<Self> {
        let ref_name = ref_name.into();
        validate_event_ref(&ref_name)?;

        if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
            return Ok(Self::Branch(BranchName::parse(branch)?));
        }

        Ok(Self::Ref(ref_name))
    }
}

impl Hash for UpdateTarget {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            Self::Branch(branch) => {
                0_u8.hash(state);
                branch.as_str().hash(state);
            }
            Self::DefaultBranch => {
                1_u8.hash(state);
            }
            Self::Commit(commit) => {
                2_u8.hash(state);
                commit.as_str().hash(state);
            }
            Self::ShortCommit(commit) => {
                3_u8.hash(state);
                commit.as_str().hash(state);
            }
            Self::Ref(ref_name) => {
                4_u8.hash(state);
                ref_name.hash(state);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UpdateKey {
    pub repo: RepoKey,
    pub target: UpdateTarget,
}

impl UpdateKey {
    pub fn new(repo: RepoKey, target: UpdateTarget) -> Self {
        Self { repo, target }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRequest {
    pub repo: RepoKey,
    pub target: UpdateTarget,
    pub source: UpdateSource,
}

impl UpdateRequest {
    pub fn key(&self) -> UpdateKey {
        UpdateKey::new(self.repo.clone(), self.target.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateDisposition {
    Updated,
    LeaseBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub key: UpdateKey,
    pub source: UpdateSource,
    pub disposition: UpdateDisposition,
}

impl UpdateOutcome {
    fn updated(request: &UpdateRequest) -> Self {
        Self {
            key: request.key(),
            source: request.source,
            disposition: UpdateDisposition::Updated,
        }
    }

    fn lease_busy(request: &UpdateRequest) -> Self {
        Self {
            key: request.key(),
            source: request.source,
            disposition: UpdateDisposition::LeaseBusy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateHint {
    Cron { repo: RepoKey, selector: Selector },
    ReadThrough { repo: RepoKey, selector: Selector },
    Event { repo: RepoKey, ref_name: String },
}

impl UpdateHint {
    pub fn into_request(self) -> Result<UpdateRequest> {
        match self {
            Self::Cron { repo, selector } => Ok(UpdateRequest {
                repo,
                target: UpdateTarget::from_selector(&selector),
                source: UpdateSource::Cron,
            }),
            Self::ReadThrough { repo, selector } => Ok(UpdateRequest {
                repo,
                target: UpdateTarget::from_selector(&selector),
                source: UpdateSource::ReadThrough,
            }),
            Self::Event { repo, ref_name } => Ok(UpdateRequest {
                repo,
                target: UpdateTarget::from_event_ref(ref_name)?,
                source: UpdateSource::Event,
            }),
        }
    }
}

#[async_trait]
pub trait UpdateExecutor: Send + Sync {
    async fn update(&self, request: UpdateRequest) -> Result<()>;
}

#[async_trait]
pub trait RepoLeaseManager: Send + Sync {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire>;
}

#[async_trait]
pub trait RepoLease: Send + Sync {
    async fn release(self: Box<Self>) -> Result<()>;
}

pub enum LeaseAcquire {
    Acquired(Box<dyn RepoLease>),
    Busy,
}

#[derive(Debug, Default)]
pub struct NoopUpdateExecutor;

#[async_trait]
impl UpdateExecutor for NoopUpdateExecutor {
    async fn update(&self, _request: UpdateRequest) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct InMemoryRepoLeaseManager {
    held: Arc<StdMutex<HashSet<RepoKey>>>,
}

impl InMemoryRepoLeaseManager {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RepoLeaseManager for InMemoryRepoLeaseManager {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire> {
        let mut held = self
            .held
            .lock()
            .map_err(|_| GitCacheError::Conflict("in-memory lease lock poisoned".into()))?;

        if held.contains(repo) {
            return Ok(LeaseAcquire::Busy);
        }

        held.insert(repo.clone());
        Ok(LeaseAcquire::Acquired(Box::new(InMemoryRepoLease {
            repo: repo.clone(),
            held: Arc::clone(&self.held),
            released: false,
        })))
    }
}

struct InMemoryRepoLease {
    repo: RepoKey,
    held: Arc<StdMutex<HashSet<RepoKey>>>,
    released: bool,
}

#[async_trait]
impl RepoLease for InMemoryRepoLease {
    async fn release(mut self: Box<Self>) -> Result<()> {
        self.release_sync()?;
        Ok(())
    }
}

impl InMemoryRepoLease {
    fn release_sync(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }

        let mut held = self
            .held
            .lock()
            .map_err(|_| GitCacheError::Conflict("in-memory lease lock poisoned".into()))?;
        held.remove(&self.repo);
        self.released = true;
        Ok(())
    }
}

impl Drop for InMemoryRepoLease {
    fn drop(&mut self) {
        let _ = self.release_sync();
    }
}

#[derive(Clone)]
pub struct UpdateCoordinator {
    inner: Arc<UpdateCoordinatorInner>,
}

struct UpdateCoordinatorInner {
    executor: Arc<dyn UpdateExecutor>,
    leases: Arc<dyn RepoLeaseManager>,
    inflight: Mutex<HashMap<UpdateKey, Arc<InflightUpdate>>>,
}

impl UpdateCoordinator {
    pub fn new(executor: Arc<dyn UpdateExecutor>, leases: Arc<dyn RepoLeaseManager>) -> Self {
        Self {
            inner: Arc::new(UpdateCoordinatorInner {
                executor,
                leases,
                inflight: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub async fn process_hint(&self, hint: UpdateHint) -> Result<UpdateOutcome> {
        self.update(hint.into_request()?).await
    }

    pub async fn read_through(&self, repo: RepoKey, selector: Selector) -> Result<UpdateOutcome> {
        self.process_hint(UpdateHint::ReadThrough { repo, selector })
            .await
    }

    pub async fn cron(&self, repo: RepoKey, selector: Selector) -> Result<UpdateOutcome> {
        self.process_hint(UpdateHint::Cron { repo, selector }).await
    }

    pub async fn event_hint(
        &self,
        repo: RepoKey,
        ref_name: impl Into<String>,
    ) -> Result<UpdateOutcome> {
        self.process_hint(UpdateHint::Event {
            repo,
            ref_name: ref_name.into(),
        })
        .await
    }

    async fn update(&self, request: UpdateRequest) -> Result<UpdateOutcome> {
        let key = request.key();
        let (inflight, owner) = self.inflight_for(key.clone()).await;

        if !owner {
            debug!(?key, "joining in-flight update");
            return inflight.wait().await.map_err(shared_update_error);
        }

        debug!(?key, "starting update");
        let result = self.execute_with_lease(request).await;
        inflight.complete(result_to_shared(&result)).await;
        self.remove_inflight(&key, &inflight).await;
        result
    }

    async fn inflight_for(&self, key: UpdateKey) -> (Arc<InflightUpdate>, bool) {
        let mut inflight = self.inner.inflight.lock().await;
        if let Some(existing) = inflight.get(&key) {
            return (Arc::clone(existing), false);
        }

        let entry = Arc::new(InflightUpdate::new());
        inflight.insert(key, Arc::clone(&entry));
        (entry, true)
    }

    async fn execute_with_lease(&self, request: UpdateRequest) -> Result<UpdateOutcome> {
        let lease = match self.inner.leases.acquire(&request.repo).await? {
            LeaseAcquire::Acquired(lease) => lease,
            LeaseAcquire::Busy => return Ok(UpdateOutcome::lease_busy(&request)),
        };

        let update_result = self.inner.executor.update(request.clone()).await;
        let release_result = lease.release().await;

        match (update_result, release_result) {
            (Ok(()), Ok(())) => Ok(UpdateOutcome::updated(&request)),
            (Err(err), Ok(())) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Err(err), Err(release_err)) => {
                warn!(%release_err, "failed to release repo lease after update error");
                Err(err)
            }
        }
    }

    async fn remove_inflight(&self, key: &UpdateKey, entry: &Arc<InflightUpdate>) {
        let mut inflight = self.inner.inflight.lock().await;
        if inflight
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            inflight.remove(key);
        }
    }
}

struct InflightUpdate {
    sender: watch::Sender<Option<SharedUpdateResult>>,
    receiver: watch::Receiver<Option<SharedUpdateResult>>,
}

type SharedUpdateResult = std::result::Result<UpdateOutcome, String>;

impl InflightUpdate {
    fn new() -> Self {
        let (sender, receiver) = watch::channel(None);
        Self { sender, receiver }
    }

    async fn wait(&self) -> SharedUpdateResult {
        let mut receiver = self.receiver.clone();

        loop {
            if let Some(result) = receiver.borrow().clone() {
                return result;
            }

            if receiver.changed().await.is_err() {
                return Err("in-flight update owner exited before publishing a result".into());
            }
        }
    }

    async fn complete(&self, result: SharedUpdateResult) {
        let _ = self.sender.send(Some(result));
    }
}

#[derive(Clone)]
pub struct ReadThroughUpdatePath {
    coordinator: UpdateCoordinator,
}

impl ReadThroughUpdatePath {
    pub fn new(coordinator: UpdateCoordinator) -> Self {
        Self { coordinator }
    }

    pub async fn update(&self, repo: RepoKey, selector: Selector) -> Result<UpdateOutcome> {
        self.coordinator.read_through(repo, selector).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronJob {
    pub repo: RepoKey,
    pub selector: Selector,
}

impl CronJob {
    pub fn new(repo: RepoKey, selector: Selector) -> Self {
        Self { repo, selector }
    }
}

#[derive(Debug, Clone)]
pub struct CronUpdateConfig {
    pub interval: Duration,
    pub jobs: Vec<CronJob>,
    pub run_immediately: bool,
}

impl CronUpdateConfig {
    pub fn try_new(interval: Duration, jobs: Vec<CronJob>) -> Result<Self> {
        if interval.is_zero() {
            return Err(GitCacheError::Validation(
                "cron update interval must be greater than zero".into(),
            ));
        }

        Ok(Self {
            interval,
            jobs,
            run_immediately: false,
        })
    }

    pub fn with_run_immediately(mut self, run_immediately: bool) -> Self {
        self.run_immediately = run_immediately;
        self
    }
}

#[derive(Clone)]
pub struct CronUpdateLoop {
    coordinator: UpdateCoordinator,
    config: CronUpdateConfig,
}

impl CronUpdateLoop {
    pub fn new(coordinator: UpdateCoordinator, config: CronUpdateConfig) -> Self {
        Self {
            coordinator,
            config,
        }
    }

    pub async fn tick_once(&self) -> Result<Vec<UpdateOutcome>> {
        let mut outcomes = Vec::with_capacity(self.config.jobs.len());

        for job in &self.config.jobs {
            outcomes.push(
                self.coordinator
                    .cron(job.repo.clone(), job.selector.clone())
                    .await?,
            );
        }

        Ok(outcomes)
    }

    pub async fn run_until(self, mut stop: StopSignal) -> Result<()> {
        if self.config.run_immediately {
            self.tick_once().await?;
        }

        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = time::sleep(self.config.interval) => {
                    self.tick_once().await?;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventHint {
    pub repo: RepoKey,
    pub ref_name: String,
}

impl EventHint {
    pub fn new(repo: RepoKey, ref_name: impl Into<String>) -> Result<Self> {
        let ref_name = ref_name.into();
        validate_event_ref(&ref_name)?;
        Ok(Self { repo, ref_name })
    }
}

#[async_trait]
pub trait EventHintSink: Send + Sync {
    async fn submit(&self, hint: EventHint) -> Result<()>;
}

#[async_trait]
pub trait EventHintSource: Send {
    async fn next_hint(&mut self) -> Result<Option<EventHint>>;
}

#[derive(Clone)]
pub struct EventHintSender {
    sender: mpsc::Sender<EventHint>,
}

pub struct EventHintReceiver {
    receiver: mpsc::Receiver<EventHint>,
}

pub fn event_hint_channel(capacity: usize) -> (EventHintSender, EventHintReceiver) {
    let (sender, receiver) = mpsc::channel(capacity);
    (EventHintSender { sender }, EventHintReceiver { receiver })
}

#[async_trait]
impl EventHintSink for EventHintSender {
    async fn submit(&self, hint: EventHint) -> Result<()> {
        self.sender
            .send(hint)
            .await
            .map_err(|_| GitCacheError::Conflict("event hint receiver is closed".into()))
    }
}

#[async_trait]
impl EventHintSource for EventHintReceiver {
    async fn next_hint(&mut self) -> Result<Option<EventHint>> {
        Ok(self.receiver.recv().await)
    }
}

pub struct EventHintIntake<S> {
    source: S,
    coordinator: UpdateCoordinator,
}

impl<S> EventHintIntake<S>
where
    S: EventHintSource,
{
    pub fn new(source: S, coordinator: UpdateCoordinator) -> Self {
        Self {
            source,
            coordinator,
        }
    }

    pub async fn drain_once(&mut self) -> Result<Option<UpdateOutcome>> {
        let Some(hint) = self.source.next_hint().await? else {
            return Ok(None);
        };

        self.coordinator
            .event_hint(hint.repo, hint.ref_name)
            .await
            .map(Some)
    }

    pub async fn run_until(mut self, mut stop: StopSignal) -> Result<()> {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                hint = self.source.next_hint() => {
                    let Some(hint) = hint? else {
                        return Ok(());
                    };
                    self.coordinator.event_hint(hint.repo, hint.ref_name).await?;
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct Worker {
    coordinator: UpdateCoordinator,
}

impl Worker {
    pub fn new() -> Self {
        Self::with_parts(
            Arc::new(NoopUpdateExecutor),
            Arc::new(InMemoryRepoLeaseManager::new()),
        )
    }

    pub fn with_parts(
        executor: Arc<dyn UpdateExecutor>,
        leases: Arc<dyn RepoLeaseManager>,
    ) -> Self {
        Self {
            coordinator: UpdateCoordinator::new(executor, leases),
        }
    }

    pub fn coordinator(&self) -> UpdateCoordinator {
        self.coordinator.clone()
    }

    pub async fn handle_hint(&self, hint: UpdateHint) -> Result<UpdateOutcome> {
        self.coordinator.process_hint(hint).await
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct StopHandle {
    sender: watch::Sender<bool>,
}

pub struct StopSignal {
    receiver: watch::Receiver<bool>,
}

pub fn stop_channel() -> (StopHandle, StopSignal) {
    let (sender, receiver) = watch::channel(false);
    (StopHandle { sender }, StopSignal { receiver })
}

impl StopHandle {
    pub fn stop(&self) {
        let _ = self.sender.send(true);
    }
}

impl StopSignal {
    async fn cancelled(&mut self) {
        loop {
            if *self.receiver.borrow() {
                return;
            }

            if self.receiver.changed().await.is_err() {
                return;
            }
        }
    }
}

fn result_to_shared(result: &Result<UpdateOutcome>) -> SharedUpdateResult {
    result
        .as_ref()
        .cloned()
        .map_err(std::string::ToString::to_string)
}

fn shared_update_error(message: String) -> GitCacheError {
    GitCacheError::Conflict(format!("in-flight update failed: {message}"))
}

fn validate_event_ref(ref_name: &str) -> Result<()> {
    if ref_name.is_empty() {
        return Err(GitCacheError::Validation("event ref is empty".into()));
    }

    if ref_name.bytes().any(|byte| byte.is_ascii_control())
        || ref_name.contains('\\')
        || ref_name.contains("..")
        || ref_name.ends_with(".lock")
    {
        return Err(GitCacheError::Validation(format!(
            "event ref `{ref_name}` is not safe to process"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
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
            if self.calls() == 0 {
                self.started.notified().await;
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

        eprintln!(
            "high-volume dedup: {concurrent_calls} callers, 1 executor call, {elapsed:?}"
        );
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
                    coordinator
                        .read_through(repo(), branch(&branch_name))
                        .await
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
}
