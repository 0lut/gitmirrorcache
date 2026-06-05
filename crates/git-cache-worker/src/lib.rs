use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
pub use git_cache_core::{
    validate_event_ref, UpdateDisposition, UpdateExecutor, UpdateKey, UpdateOutcome, UpdateRequest,
    UpdateResult, UpdateSource, UpdateTarget,
};
#[cfg(test)]
use git_cache_core::{BranchName, CommitSha, ShortCommitSha};
use git_cache_core::{GitCacheError, LeaseConfig, RepoKey, Result, Selector};
use git_cache_objectstore::{
    acquire_lease_with_token, read_lease_with_version, release_lease_if_token_matches,
    renew_lease_if_token_matches, steal_expired_lease_if_version_matches, LeaseManifest,
    ObjectStore,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tracing::{debug, warn};

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
                lease_token: None,
            }),
            Self::ReadThrough { repo, selector } => Ok(UpdateRequest {
                repo,
                target: UpdateTarget::from_selector(&selector),
                source: UpdateSource::ReadThrough,
                lease_token: None,
            }),
            Self::Event { repo, ref_name } => Ok(UpdateRequest {
                repo,
                target: UpdateTarget::from_event_ref(ref_name)?,
                source: UpdateSource::Event,
                lease_token: None,
            }),
        }
    }
}

#[async_trait]
pub trait RepoLeaseManager: Send + Sync {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire>;
}

#[async_trait]
pub trait RepoLease: Send + Sync {
    fn token(&self) -> &str;
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
    async fn update(&self, _request: UpdateRequest) -> Result<UpdateResult> {
        Ok(UpdateResult::default())
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
    fn token(&self) -> &str {
        ""
    }

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
pub struct ObjectStoreRepoLeaseManager {
    store: Arc<dyn ObjectStore>,
    holder: String,
    ttl: ChronoDuration,
    renew_interval: Duration,
    steal_skew: ChronoDuration,
}

impl ObjectStoreRepoLeaseManager {
    pub fn new(store: Arc<dyn ObjectStore>, config: &LeaseConfig) -> Self {
        let holder = config.worker_id.clone().unwrap_or_else(default_holder_id);
        Self {
            store,
            holder,
            ttl: ChronoDuration::seconds(config.ttl_seconds as i64),
            renew_interval: Duration::from_secs(config.renew_interval_seconds.max(1)),
            steal_skew: ChronoDuration::seconds(config.steal_skew_seconds as i64),
        }
    }

    fn lease_manifest(
        &self,
        repo: &RepoKey,
        token: String,
        now: chrono::DateTime<Utc>,
    ) -> LeaseManifest {
        LeaseManifest {
            schema_version: 1,
            repo: repo.clone(),
            name: "repo-write".into(),
            holder: self.holder.clone(),
            token,
            acquired_at: now,
            renewed_at: Some(now),
            expires_at: now + self.ttl,
            released_at: None,
            operation: Some("repo-write".into()),
            expected_head: None,
        }
    }
}

#[async_trait]
impl RepoLeaseManager for ObjectStoreRepoLeaseManager {
    async fn acquire(&self, repo: &RepoKey) -> Result<LeaseAcquire> {
        let now = Utc::now();
        let token = uuid::Uuid::now_v7().to_string();
        if let Some(lease) = acquire_lease_with_token(
            &*self.store,
            repo,
            "repo-write",
            self.holder.clone(),
            token.clone(),
            now,
            self.ttl,
        )
        .await?
        {
            return Ok(LeaseAcquire::Acquired(Box::new(ObjectStoreRepoLease::new(
                Arc::clone(&self.store),
                repo.clone(),
                lease.name.clone(),
                lease.token,
                self.ttl,
                self.renew_interval,
            ))));
        }

        let Some((existing, version)) =
            read_lease_with_version(&*self.store, repo, "repo-write").await?
        else {
            return Err(GitCacheError::LeaseStealConflict(format!(
                "repo-write lease for `{repo}` disappeared during acquisition"
            )));
        };

        let expired = existing.released_at.is_some() || {
            let renewed_at_by_holder = existing.renewed_at.unwrap_or(existing.acquired_at);
            let ttl_at_write = existing.expires_at - renewed_at_by_holder;
            if let Some(obj_updated) = version.updated_at {
                // Use object-store metadata time to avoid inter-worker clock skew.
                // obj_updated is the server-side timestamp of the last lease write;
                // the lease should have expired ttl_at_write after that moment.
                let elapsed = now - obj_updated;
                elapsed > ttl_at_write + self.steal_skew
            } else {
                now > existing.expires_at + self.steal_skew
            }
        };
        if !expired {
            return Ok(LeaseAcquire::Busy);
        }

        let stolen = self.lease_manifest(repo, token, now);
        if !steal_expired_lease_if_version_matches(
            &*self.store,
            repo,
            "repo-write",
            &version,
            stolen.clone(),
        )
        .await?
        {
            return Ok(LeaseAcquire::Busy);
        }

        Ok(LeaseAcquire::Acquired(Box::new(ObjectStoreRepoLease::new(
            Arc::clone(&self.store),
            repo.clone(),
            stolen.name,
            stolen.token,
            self.ttl,
            self.renew_interval,
        ))))
    }
}

struct ObjectStoreRepoLease {
    store: Arc<dyn ObjectStore>,
    repo: RepoKey,
    name: String,
    token: String,
    renew_task: Option<JoinHandle<()>>,
}

impl ObjectStoreRepoLease {
    fn new(
        store: Arc<dyn ObjectStore>,
        repo: RepoKey,
        name: String,
        token: String,
        ttl: ChronoDuration,
        renew_interval: Duration,
    ) -> Self {
        let renew_task = spawn_lease_renewal(
            Arc::clone(&store),
            repo.clone(),
            name.clone(),
            token.clone(),
            ttl,
            renew_interval,
        );
        Self {
            store,
            repo,
            name,
            token,
            renew_task: Some(renew_task),
        }
    }
}

impl Drop for ObjectStoreRepoLease {
    fn drop(&mut self) {
        if let Some(renew_task) = &self.renew_task {
            renew_task.abort();
        }
    }
}

#[async_trait]
impl RepoLease for ObjectStoreRepoLease {
    fn token(&self) -> &str {
        &self.token
    }

    async fn release(mut self: Box<Self>) -> Result<()> {
        if let Some(renew_task) = self.renew_task.take() {
            renew_task.abort();
            let _ = renew_task.await;
        }
        if release_lease_if_token_matches(
            &*self.store,
            &self.repo,
            &self.name,
            &self.token,
            Utc::now(),
        )
        .await?
        {
            Ok(())
        } else {
            Err(GitCacheError::LeaseLost(format!(
                "repo-write lease for `{}` was lost before release",
                self.repo
            )))
        }
    }
}

fn spawn_lease_renewal(
    store: Arc<dyn ObjectStore>,
    repo: RepoKey,
    name: String,
    token: String,
    ttl: ChronoDuration,
    renew_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(renew_interval);
        loop {
            interval.tick().await;
            match renew_lease_if_token_matches(&*store, &repo, &name, &token, Utc::now(), ttl).await
            {
                Ok(true) => {}
                Ok(false) => {
                    warn!(%repo, lease = %name, "stopping lease renewal after token mismatch");
                    return;
                }
                Err(err) => warn!(%repo, lease = %name, %err, "failed to renew repo lease"),
            }
        }
    })
}

fn default_holder_id() -> String {
    let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".into());
    format!(
        "{hostname}/pid-{}/{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    )
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
        let coordinator = self.clone();
        let join_result =
            tokio::spawn(async move { coordinator.execute_with_lease(request).await }).await;

        let result = match join_result {
            Ok(r) => r,
            Err(join_err) if join_err.is_panic() => {
                warn!(?key, "executor panicked during update");
                Err(GitCacheError::Internal(
                    "executor panicked during update".into(),
                ))
            }
            Err(join_err) => {
                warn!(?key, %join_err, "update task cancelled");
                Err(GitCacheError::Internal("update task cancelled".into()))
            }
        };

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

        let mut request = request;
        request.lease_token = Some(lease.token().to_string());
        let update_result = self.inner.executor.update(request.clone()).await;
        let release_result = lease.release().await;

        match (update_result, release_result) {
            (Ok(result), Ok(())) => Ok(UpdateOutcome::updated(&request, result)),
            (Err(err), Ok(())) => Err(err),
            (Ok(_), Err(err)) => Err(err),
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionCleanupReport {
    pub sessions_removed: usize,
    pub errors: Vec<String>,
}

#[async_trait]
pub trait SessionCleaner: Send + Sync {
    async fn cleanup_expired_sessions(&self) -> Result<SessionCleanupReport>;
}

#[derive(Clone)]
pub struct SessionCleanupLoop {
    cleaner: Arc<dyn SessionCleaner>,
    interval: Duration,
}

impl SessionCleanupLoop {
    pub fn new(cleaner: Arc<dyn SessionCleaner>, interval: Duration) -> Result<Self> {
        if interval.is_zero() {
            return Err(GitCacheError::Validation(
                "session cleanup interval must be greater than zero".into(),
            ));
        }

        Ok(Self { cleaner, interval })
    }

    pub async fn tick_once(&self) -> Result<SessionCleanupReport> {
        self.cleaner.cleanup_expired_sessions().await
    }

    pub async fn run_until(self, mut stop: StopSignal) -> Result<()> {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                _ = time::sleep(self.interval) => {
                    match self.tick_once().await {
                        Ok(report) => {
                            debug!(
                                sessions_removed = report.sessions_removed,
                                errors = report.errors.len(),
                                "session cleanup completed"
                            );
                        }
                        Err(err) => {
                            warn!(%err, "session cleanup failed");
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use git_cache_objectstore::{LocalObjectStore, ObjectStore as TestObjectStore};
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
        async fn update(&self, request: UpdateRequest) -> Result<UpdateResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().await.push(request);
            self.started.notify_waiters();

            if self.hold {
                self.release.notified().await;
            }

            Ok(UpdateResult::default())
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

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("git-cache-worker-test-{}", uuid::Uuid::now_v7()))
    }

    fn lease_config(worker_id: &str) -> LeaseConfig {
        LeaseConfig {
            worker_id: Some(worker_id.into()),
            ttl_seconds: 60,
            renew_interval_seconds: 60,
            steal_skew_seconds: 0,
            busy_retry_after_seconds: 1,
        }
    }

    #[tokio::test]
    async fn object_store_repo_lease_manager_reports_busy_for_held_lease() {
        let root = temp_root();
        let store: Arc<dyn TestObjectStore> = Arc::new(LocalObjectStore::new(&root));
        let manager_a = ObjectStoreRepoLeaseManager::new(store.clone(), &lease_config("worker-a"));
        let manager_b = ObjectStoreRepoLeaseManager::new(store.clone(), &lease_config("worker-b"));

        let LeaseAcquire::Acquired(lease) = manager_a.acquire(&repo()).await.unwrap() else {
            panic!("expected lease");
        };
        assert!(matches!(
            manager_b.acquire(&repo()).await.unwrap(),
            LeaseAcquire::Busy
        ));

        lease.release().await.unwrap();
        let LeaseAcquire::Acquired(lease) = manager_b.acquire(&repo()).await.unwrap() else {
            panic!("expected released lease to be reusable");
        };
        lease.release().await.unwrap();

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn object_store_repo_lease_manager_steals_expired_lease() {
        let root = temp_root();
        let store: Arc<dyn TestObjectStore> = Arc::new(LocalObjectStore::new(&root));
        let short = LeaseConfig {
            worker_id: Some("worker-a".into()),
            ttl_seconds: 0,
            renew_interval_seconds: 60,
            steal_skew_seconds: 0,
            busy_retry_after_seconds: 1,
        };
        let manager_a = ObjectStoreRepoLeaseManager::new(store.clone(), &short);
        let steal = LeaseConfig {
            worker_id: Some("worker-b".into()),
            ..short.clone()
        };
        let manager_b = ObjectStoreRepoLeaseManager::new(store.clone(), &steal);

        let LeaseAcquire::Acquired(lease_a) = manager_a.acquire(&repo()).await.unwrap() else {
            panic!("expected lease");
        };
        let LeaseAcquire::Acquired(lease_b) = manager_b.acquire(&repo()).await.unwrap() else {
            panic!("expected expired lease steal");
        };
        assert!(lease_a.release().await.is_err());
        lease_b.release().await.unwrap();

        let _ = tokio::fs::remove_dir_all(root).await;
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

    // ── SessionCleanupLoop tests ────────────────────────────────────────

    #[derive(Default)]
    struct RecordingCleaner {
        calls: AtomicUsize,
        sessions_to_remove: AtomicUsize,
    }

    impl RecordingCleaner {
        fn new(sessions_to_remove: usize) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                sessions_to_remove: AtomicUsize::new(sessions_to_remove),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SessionCleaner for RecordingCleaner {
        async fn cleanup_expired_sessions(&self) -> Result<SessionCleanupReport> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(SessionCleanupReport {
                sessions_removed: self.sessions_to_remove.load(Ordering::SeqCst),
                errors: vec![],
            })
        }
    }

    #[test]
    fn session_cleanup_loop_rejects_zero_interval() {
        let cleaner: Arc<dyn SessionCleaner> = Arc::new(RecordingCleaner::new(0));
        let result = SessionCleanupLoop::new(cleaner, Duration::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn session_cleanup_loop_accepts_nonzero_interval() {
        let cleaner: Arc<dyn SessionCleaner> = Arc::new(RecordingCleaner::new(0));
        let loop_ = SessionCleanupLoop::new(cleaner, Duration::from_secs(60)).unwrap();
        assert_eq!(loop_.interval, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn session_cleanup_loop_tick_once_delegates_to_cleaner() {
        let cleaner = Arc::new(RecordingCleaner::new(5));
        let cleaner_dyn: Arc<dyn SessionCleaner> = Arc::clone(&cleaner) as _;
        let loop_ = SessionCleanupLoop::new(cleaner_dyn, Duration::from_secs(60)).unwrap();
        let report = loop_.tick_once().await.unwrap();
        assert_eq!(report.sessions_removed, 5);
        assert!(report.errors.is_empty());
        assert_eq!(cleaner.calls(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn session_cleanup_loop_ticks_on_interval_and_stops() {
        let cleaner = Arc::new(RecordingCleaner::new(2));
        let cleaner_dyn: Arc<dyn SessionCleaner> = Arc::clone(&cleaner) as _;
        let loop_ = SessionCleanupLoop::new(cleaner_dyn, Duration::from_secs(10)).unwrap();
        let (stop, stop_signal) = stop_channel();

        let task = tokio::spawn(loop_.run_until(stop_signal));
        tokio::task::yield_now().await;
        assert_eq!(cleaner.calls(), 0);

        advance(Duration::from_secs(9)).await;
        tokio::task::yield_now().await;
        assert_eq!(cleaner.calls(), 0);

        advance(Duration::from_secs(1)).await;
        timeout(Duration::from_secs(1), async {
            while cleaner.calls() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(cleaner.calls(), 1);

        stop.stop();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn session_cleanup_loop_multiple_ticks() {
        let cleaner = Arc::new(RecordingCleaner::new(1));
        let cleaner_dyn: Arc<dyn SessionCleaner> = Arc::clone(&cleaner) as _;
        let loop_ = SessionCleanupLoop::new(cleaner_dyn, Duration::from_secs(5)).unwrap();
        let (stop, stop_signal) = stop_channel();

        let task = tokio::spawn(loop_.run_until(stop_signal));
        tokio::task::yield_now().await;

        advance(Duration::from_secs(5)).await;
        timeout(Duration::from_secs(1), async {
            while cleaner.calls() < 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        advance(Duration::from_secs(5)).await;
        timeout(Duration::from_secs(1), async {
            while cleaner.calls() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(cleaner.calls(), 2);

        stop.stop();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    struct FailingCleaner;

    #[async_trait]
    impl SessionCleaner for FailingCleaner {
        async fn cleanup_expired_sessions(&self) -> Result<SessionCleanupReport> {
            Err(GitCacheError::Internal(
                "cleanup failed intentionally".into(),
            ))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn session_cleanup_loop_continues_after_error() {
        let cleaner: Arc<dyn SessionCleaner> = Arc::new(FailingCleaner);
        let loop_ = SessionCleanupLoop::new(cleaner, Duration::from_secs(5)).unwrap();
        let (stop, stop_signal) = stop_channel();

        let task = tokio::spawn(loop_.run_until(stop_signal));

        // Advance through two ticks — the loop should survive errors.
        advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

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
        async fn update(&self, _request: UpdateRequest) -> Result<UpdateResult> {
            panic!("executor panicked intentionally");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn inflight_dedup_thundering_herd() {
        let executor = Arc::new(RecordingExecutor::new(true));
        let coord = coordinator(Arc::clone(&executor));

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

        tokio::task::yield_now().await;
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
}
