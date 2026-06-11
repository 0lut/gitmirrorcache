pub mod async_disk;

pub use async_disk::{AsyncDiskManager, AsyncReservation};

use git_cache_core::{GitCacheError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct DiskManager {
    root: PathBuf,
    quota_bytes: u64,
    min_free_bytes: u64,
    state: Arc<Mutex<DiskState>>,
    pending_accesses: Arc<Mutex<HashMap<PathBuf, u64>>>,
}

#[derive(Debug, Default)]
struct DiskState {
    active_reservations: HashMap<Uuid, ReservationMarker>,
    repo_locks: HashMap<PathBuf, usize>,
    invalidating_repos: HashSet<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskStatus {
    pub root: PathBuf,
    pub quota_bytes: u64,
    pub min_free_bytes: u64,
    pub used_bytes: u64,
    pub reserved_bytes: u64,
    pub accounted_bytes: u64,
    pub available_bytes: u64,
    pub repo_count: usize,
    pub protected_repo_count: usize,
    pub locked_repo_count: usize,
    pub evictable_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIndex {
    pub version: u32,
    pub repos: BTreeMap<PathBuf, RepoIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoIndexEntry {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub last_accessed_unix_millis: u64,
    pub protected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CleanupReport {
    pub removed_temp_dirs: usize,
    pub removed_reservation_markers: usize,
    pub freed_bytes: u64,
}

#[derive(Debug)]
pub struct Reservation {
    pub(crate) id: Uuid,
    pub(crate) bytes: u64,
    pub(crate) root: PathBuf,
    pub(crate) state: Arc<Mutex<DiskState>>,
}

#[derive(Debug)]
pub struct RepoLock {
    repo_path: PathBuf,
    state: Arc<Mutex<DiskState>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ReservationMarker {
    id: Uuid,
    bytes: u64,
    temp_dir: PathBuf,
    created_at_unix_millis: u64,
}

#[derive(Debug, Clone, Copy)]
struct Accounting {
    used_bytes: u64,
    reserved_bytes: u64,
    max_usable_bytes: u64,
}

impl DiskManager {
    pub fn new(root: impl Into<PathBuf>, quota_bytes: u64, min_free_bytes: u64) -> Self {
        Self {
            root: root.into(),
            quota_bytes,
            min_free_bytes,
            state: Arc::new(Mutex::new(DiskState::default())),
            pending_accesses: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn status(&self) -> Result<DiskStatus> {
        self.ensure_layout()?;

        let state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        let index = self.sync_repo_index_locked()?;
        let used_bytes = directory_size(&self.root)?;
        let reserved_bytes = self.reserved_bytes_locked(&state)?;
        let accounted_bytes = used_bytes.saturating_add(reserved_bytes);
        let max_usable = self.max_usable_bytes();
        let active_locks = &state.repo_locks;

        let protected_repo_count = index.repos.values().filter(|entry| entry.protected).count();
        let locked_repo_count = index
            .repos
            .keys()
            .filter(|path| active_locks.contains_key(*path))
            .count();
        let evictable_bytes = index
            .repos
            .values()
            .filter(|entry| !entry.protected && !active_locks.contains_key(&entry.path))
            .map(|entry| entry.size_bytes)
            .sum();

        Ok(DiskStatus {
            root: self.root.clone(),
            quota_bytes: self.quota_bytes,
            min_free_bytes: self.min_free_bytes,
            used_bytes,
            reserved_bytes,
            accounted_bytes,
            available_bytes: max_usable.saturating_sub(accounted_bytes),
            repo_count: index.repos.len(),
            protected_repo_count,
            locked_repo_count,
            evictable_bytes,
        })
    }

    pub fn reserve(&self, bytes: u64) -> Result<Reservation> {
        self.ensure_layout()?;

        let mut state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        self.evict_until_available_locked(bytes, &state)?;

        let accounting = self.accounting_locked(&state)?;
        let needed = accounting
            .used_bytes
            .saturating_add(accounting.reserved_bytes)
            .saturating_add(bytes);
        if needed > accounting.max_usable_bytes {
            return Err(disk_full_error(
                bytes,
                accounting.used_bytes,
                accounting.reserved_bytes,
                accounting.max_usable_bytes,
            ));
        }

        let id = Uuid::now_v7();
        let marker = ReservationMarker {
            id,
            bytes,
            temp_dir: self.temp_dir_for(id),
            created_at_unix_millis: now_unix_millis(),
        };

        fs::create_dir_all(&marker.temp_dir)?;
        write_json(&self.marker_path(id), &marker)?;
        state.active_reservations.insert(id, marker);

        Ok(Reservation {
            id,
            bytes,
            root: self.root.clone(),
            state: Arc::clone(&self.state),
        })
    }

    pub fn record_repo_access(&self, repo_path: impl AsRef<Path>) -> Result<RepoIndexEntry> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let _repo_lock = self.lock_repo(repo_path.clone())?;
        let repo_dir = self.repo_dir(&repo_path);
        let size_bytes = directory_size(&repo_dir)?;

        let _state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        let mut index = self.load_index()?;
        let protected = index
            .repos
            .get(&repo_path)
            .map(|entry| entry.protected)
            .unwrap_or(false);
        let entry = RepoIndexEntry {
            path: repo_path.clone(),
            size_bytes,
            last_accessed_unix_millis: now_unix_millis(),
            protected,
        };

        index.repos.insert(repo_path, entry.clone());
        self.write_index(&index)?;
        Ok(entry)
    }

    /// Record a repo access in memory only. The timestamp is applied to the
    /// persistent index by the next [`DiskManager::flush_repo_accesses`] call,
    /// and is consulted by eviction in the meantime so recently accessed repos
    /// are not selected as LRU victims before a flush happens.
    pub fn note_repo_access(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let mut pending = self
            .pending_accesses
            .lock()
            .map_err(|_| GitCacheError::Internal("pending accesses mutex poisoned".into()))?;
        pending.insert(repo_path, now_unix_millis());
        Ok(())
    }

    /// Apply buffered in-memory access timestamps to the persistent repo
    /// index in a single write. Returns the number of entries applied.
    ///
    /// The pending-accesses mutex is held only to take the buffered map, and
    /// the buffer lives behind its own lock, so concurrent
    /// [`DiskManager::note_repo_access`] calls never contend with index I/O.
    /// On I/O failure the taken entries are merged back into the pending map
    /// so the next flush retries them.
    pub fn flush_repo_accesses(&self) -> Result<usize> {
        self.ensure_layout()?;

        let pending = {
            let mut pending = self
                .pending_accesses
                .lock()
                .map_err(|_| GitCacheError::Internal("pending accesses mutex poisoned".into()))?;
            std::mem::take(&mut *pending)
        };
        if pending.is_empty() {
            return Ok(0);
        }

        let result = self.apply_pending_accesses(&pending);
        if result.is_err() {
            if let Ok(mut current) = self.pending_accesses.lock() {
                for (repo_path, accessed_at) in pending {
                    let entry = current.entry(repo_path).or_insert(0);
                    *entry = (*entry).max(accessed_at);
                }
            }
        }
        result
    }

    /// Index reads/writes elsewhere are serialized under the state mutex, so
    /// hold it here too while rewriting the index.
    fn apply_pending_accesses(&self, pending: &HashMap<PathBuf, u64>) -> Result<usize> {
        let _state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        let mut index = self.load_index()?;
        let mut applied = 0usize;
        for (repo_path, accessed_at) in pending {
            if let Some(entry) = index.repos.get_mut(repo_path) {
                entry.last_accessed_unix_millis = entry.last_accessed_unix_millis.max(*accessed_at);
                applied += 1;
                continue;
            }
            let repo_dir = self.repo_dir(repo_path);
            if !repo_dir.exists() {
                continue;
            }
            let entry = RepoIndexEntry {
                path: repo_path.clone(),
                size_bytes: directory_size(&repo_dir)?,
                last_accessed_unix_millis: *accessed_at,
                protected: false,
            };
            index.repos.insert(repo_path.clone(), entry);
            applied += 1;
        }
        self.write_index(&index)?;
        Ok(applied)
    }

    pub fn touch_repo_access(&self, repo_path: impl AsRef<Path>) -> Result<RepoIndexEntry> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let _repo_lock = self.lock_repo(repo_path.clone())?;
        {
            let _state = self
                .state
                .lock()
                .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
            let mut index = self.load_index()?;
            if let Some(existing) = index.repos.get(&repo_path) {
                let entry = RepoIndexEntry {
                    path: existing.path.clone(),
                    size_bytes: existing.size_bytes,
                    last_accessed_unix_millis: now_unix_millis(),
                    protected: existing.protected,
                };
                index.repos.insert(repo_path, entry.clone());
                self.write_index(&index)?;
                return Ok(entry);
            }
        }

        self.record_repo_access(repo_path)
    }

    pub fn set_repo_protected(
        &self,
        repo_path: impl AsRef<Path>,
        protected: bool,
    ) -> Result<RepoIndexEntry> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let _repo_lock = self.lock_repo(repo_path.clone())?;
        let repo_dir = self.repo_dir(&repo_path);
        let size_bytes = directory_size(&repo_dir)?;

        let _state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        let mut index = self.load_index()?;
        let last_accessed_unix_millis = index
            .repos
            .get(&repo_path)
            .map(|entry| entry.last_accessed_unix_millis)
            .unwrap_or_else(now_unix_millis);
        let entry = RepoIndexEntry {
            path: repo_path.clone(),
            size_bytes,
            last_accessed_unix_millis,
            protected,
        };

        index.repos.insert(repo_path, entry.clone());
        self.write_index(&index)?;
        Ok(entry)
    }

    pub fn invalidate_repo(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
            if state.repo_locks.contains_key(&repo_path)
                || state.invalidating_repos.contains(&repo_path)
            {
                return Err(GitCacheError::Conflict(format!(
                    "repo `{}` is currently locked",
                    repo_path.display()
                )));
            }
            state.invalidating_repos.insert(repo_path.clone());
        }

        let result = (|| {
            let repo_dir = self.repo_dir(&repo_path);
            if repo_dir.exists() {
                fs::remove_dir_all(&repo_dir)?;
            }
            Ok(())
        })();

        let cleanup_result = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))
            .and_then(|mut state| {
                let index_result = if result.is_ok() {
                    let mut index = self.load_index()?;
                    index.repos.remove(&repo_path);
                    self.write_index(&index)
                } else {
                    Ok(())
                };
                if let Ok(mut pending) = self.pending_accesses.lock() {
                    pending.remove(&repo_path);
                }
                state.invalidating_repos.remove(&repo_path);
                index_result
            });

        match (result, cleanup_result) {
            (Err(err), _) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    pub fn lock_repo(&self, repo_path: impl AsRef<Path>) -> Result<RepoLock> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        if state.invalidating_repos.contains(&repo_path) {
            return Err(GitCacheError::Conflict(format!(
                "repo `{}` is currently being invalidated",
                repo_path.display()
            )));
        }
        *state.repo_locks.entry(repo_path.clone()).or_insert(0) += 1;

        Ok(RepoLock {
            repo_path,
            state: Arc::clone(&self.state),
        })
    }

    pub fn repo_index(&self) -> Result<RepoIndex> {
        self.ensure_layout()?;

        let _state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        self.sync_repo_index_locked()
    }

    pub fn cleanup_stale_temps(&self, older_than: Duration) -> Result<CleanupReport> {
        self.ensure_layout()?;

        let state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
        let active_reservations = state
            .active_reservations
            .keys()
            .copied()
            .collect::<HashSet<_>>();
        let mut report = CleanupReport {
            removed_temp_dirs: 0,
            removed_reservation_markers: 0,
            freed_bytes: 0,
        };

        if self.reservations_dir().exists() {
            for entry in fs::read_dir(self.reservations_dir())? {
                let path = entry?.path();
                if !path.is_file() || !is_stale(&path, older_than)? {
                    continue;
                }

                let Some(id) = uuid_from_marker_path(&path) else {
                    continue;
                };
                if active_reservations.contains(&id) {
                    continue;
                }

                let temp_dir = self.temp_dir_for(id);
                if temp_dir.exists() {
                    report.freed_bytes = report
                        .freed_bytes
                        .saturating_add(directory_size(&temp_dir)?);
                    fs::remove_dir_all(temp_dir)?;
                    report.removed_temp_dirs += 1;
                }

                report.freed_bytes = report
                    .freed_bytes
                    .saturating_add(path.metadata().map(|metadata| metadata.len()).unwrap_or(0));
                fs::remove_file(path)?;
                report.removed_reservation_markers += 1;
            }
        }

        if self.tmp_dir().exists() {
            for entry in fs::read_dir(self.tmp_dir())? {
                let path = entry?.path();
                if !path.is_dir() || !is_stale(&path, older_than)? {
                    continue;
                }

                if uuid_from_dir_name(path.file_name())
                    .map(|id| active_reservations.contains(&id))
                    .unwrap_or(false)
                {
                    continue;
                }

                report.freed_bytes = report.freed_bytes.saturating_add(directory_size(&path)?);
                fs::remove_dir_all(path)?;
                report.removed_temp_dirs += 1;
            }
        }

        Ok(report)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn reservations_dir(&self) -> PathBuf {
        self.root.join("reservations")
    }

    pub fn index_dir(&self) -> PathBuf {
        self.root.join("index")
    }

    fn ensure_layout(&self) -> Result<()> {
        fs::create_dir_all(self.repos_dir())?;
        fs::create_dir_all(self.tmp_dir())?;
        fs::create_dir_all(self.reservations_dir())?;
        fs::create_dir_all(self.index_dir())?;
        Ok(())
    }

    fn max_usable_bytes(&self) -> u64 {
        self.quota_bytes.saturating_sub(self.min_free_bytes)
    }

    fn temp_dir_for(&self, id: Uuid) -> PathBuf {
        self.tmp_dir().join(id.to_string())
    }

    fn marker_path(&self, id: Uuid) -> PathBuf {
        self.reservations_dir().join(format!("{id}.json"))
    }

    fn repo_dir(&self, repo_path: &Path) -> PathBuf {
        self.repos_dir().join(repo_path)
    }

    fn index_path(&self) -> PathBuf {
        self.index_dir().join("repo-index.json")
    }

    fn accounting_locked(&self, state: &DiskState) -> Result<Accounting> {
        Ok(Accounting {
            used_bytes: directory_size(&self.root)?,
            reserved_bytes: self.reserved_bytes_locked(state)?,
            max_usable_bytes: self.max_usable_bytes(),
        })
    }

    fn reserved_bytes_locked(&self, state: &DiskState) -> Result<u64> {
        let mut reservations = HashMap::new();

        for marker in self.read_reservation_markers()? {
            reservations.insert(marker.id, marker.bytes);
        }
        for marker in state.active_reservations.values() {
            reservations.insert(marker.id, marker.bytes);
        }

        Ok(reservations.values().copied().sum())
    }

    fn read_reservation_markers(&self) -> Result<Vec<ReservationMarker>> {
        if !self.reservations_dir().exists() {
            return Ok(Vec::new());
        }

        let mut markers = Vec::new();
        for entry in fs::read_dir(self.reservations_dir())? {
            let path = entry?.path();
            if path.is_file() {
                markers.push(read_reservation_marker(&path, &self.tmp_dir())?);
            }
        }

        Ok(markers)
    }

    fn evict_until_available_locked(&self, bytes: u64, state: &DiskState) -> Result<()> {
        let mut index = self.sync_repo_index_locked()?;
        {
            let pending = self
                .pending_accesses
                .lock()
                .map_err(|_| GitCacheError::Internal("pending accesses mutex poisoned".into()))?;
            for (repo_path, accessed_at) in pending.iter() {
                if let Some(entry) = index.repos.get_mut(repo_path) {
                    entry.last_accessed_unix_millis =
                        entry.last_accessed_unix_millis.max(*accessed_at);
                }
            }
        }

        loop {
            let accounting = self.accounting_locked(state)?;
            let needed = accounting
                .used_bytes
                .saturating_add(accounting.reserved_bytes)
                .saturating_add(bytes);

            if needed <= accounting.max_usable_bytes {
                return Ok(());
            }

            let Some(victim) =
                lru_evictable_repo(&index, &state.repo_locks, &state.invalidating_repos)
            else {
                return Err(disk_full_error(
                    bytes,
                    accounting.used_bytes,
                    accounting.reserved_bytes,
                    accounting.max_usable_bytes,
                ));
            };

            let victim_dir = self.repo_dir(&victim);
            if victim_dir.exists() {
                fs::remove_dir_all(&victim_dir)?;
            }
            index.repos.remove(&victim);
            self.write_index(&index)?;
        }
    }

    fn sync_repo_index_locked(&self) -> Result<RepoIndex> {
        let mut index = self.load_index()?;
        let discovered = discover_repos(&self.repos_dir())?;
        let mut synced = BTreeMap::new();

        for repo_path in discovered {
            let repo_dir = self.repo_dir(&repo_path);
            let existing = index.repos.remove(&repo_path);
            let entry = RepoIndexEntry {
                path: repo_path.clone(),
                size_bytes: directory_size(&repo_dir)?,
                last_accessed_unix_millis: existing
                    .as_ref()
                    .map(|entry| entry.last_accessed_unix_millis)
                    .unwrap_or_else(now_unix_millis),
                protected: existing.map(|entry| entry.protected).unwrap_or(false),
            };
            synced.insert(repo_path, entry);
        }

        index.repos = synced;
        self.write_index(&index)?;
        Ok(index)
    }

    fn load_index(&self) -> Result<RepoIndex> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(RepoIndex {
                version: INDEX_VERSION,
                repos: BTreeMap::new(),
            });
        }

        let index: RepoIndex = read_json(&path)?;
        if index.version != INDEX_VERSION {
            return Err(GitCacheError::Validation(format!(
                "unsupported repo index version {}; expected {INDEX_VERSION}",
                index.version
            )));
        }

        Ok(index)
    }

    fn write_index(&self, index: &RepoIndex) -> Result<()> {
        write_json(&self.index_path(), index)
    }
}

impl Reservation {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn temp_path(&self) -> PathBuf {
        self.root.join("tmp").join(self.id.to_string())
    }

    fn marker_path(&self) -> PathBuf {
        self.root
            .join("reservations")
            .join(format!("{}.json", self.id))
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.marker_path());
        let _ = fs::remove_dir_all(self.temp_path());
        if let Ok(mut state) = self.state.lock() {
            state.active_reservations.remove(&self.id);
        }
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(count) = state.repo_locks.get_mut(&self.repo_path) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.repo_locks.remove(&self.repo_path);
                }
            }
        }
    }
}

fn disk_full_error(requested: u64, used: u64, reserved: u64, max_usable: u64) -> GitCacheError {
    GitCacheError::DiskFull(format!(
        "requested {requested} bytes, used {used}, reserved {reserved}, max usable {max_usable}"
    ))
}

fn directory_size(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }

    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];

    while let Some(path) = stack.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            let entries = match fs::read_dir(&path) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                match entry {
                    Ok(e) => stack.push(e.path()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }

    Ok(total)
}

fn discover_repos(repos_dir: &Path) -> Result<Vec<PathBuf>> {
    if !repos_dir.exists() {
        return Ok(Vec::new());
    }

    let mut repos = Vec::new();
    let mut stack = vec![repos_dir.to_path_buf()];

    while let Some(path) = stack.pop() {
        let entries = match fs::read_dir(&path) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let path = match entry {
                Ok(e) => e.path(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if !path.is_dir() {
                continue;
            }

            if path.extension() == Some(OsStr::new("git")) {
                let repo_path = path.strip_prefix(repos_dir).map_err(|err| {
                    GitCacheError::Validation(format!("invalid repo path: {err}"))
                })?;
                repos.push(repo_path.to_path_buf());
            } else {
                stack.push(path);
            }
        }
    }

    repos.sort();
    Ok(repos)
}

fn lru_evictable_repo(
    index: &RepoIndex,
    locks: &HashMap<PathBuf, usize>,
    invalidating_repos: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    index
        .repos
        .values()
        .filter(|entry| {
            !entry.protected
                && !locks.contains_key(&entry.path)
                && !invalidating_repos.contains(&entry.path)
        })
        .min_by_key(|entry| entry.last_accessed_unix_millis)
        .map(|entry| entry.path.clone())
}

fn normalize_repo_path(repos_dir: &Path, path: &Path) -> Result<PathBuf> {
    let relative = if path.is_absolute() {
        path.strip_prefix(repos_dir).map_err(|_| {
            GitCacheError::Validation(format!(
                "repo path {} is outside {}",
                path.display(),
                repos_dir.display()
            ))
        })?
    } else {
        path
    };

    let mut clean = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => {
                return Err(GitCacheError::Validation(format!(
                    "repo path {} must be relative and must not contain traversal",
                    path.display()
                )));
            }
        }
    }

    if clean.as_os_str().is_empty() {
        return Err(GitCacheError::Validation(
            "repo path cannot be empty".into(),
        ));
    }

    Ok(clean)
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_reservation_marker(path: &Path, tmp_dir: &Path) -> Result<ReservationMarker> {
    let bytes = fs::read(path)?;
    if let Ok(marker) = serde_json::from_slice(&bytes) {
        return Ok(marker);
    }

    let id = uuid_from_marker_path(path).ok_or_else(|| {
        GitCacheError::Validation(format!(
            "invalid reservation marker path {}",
            path.display()
        ))
    })?;
    let bytes = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .ok_or_else(|| {
            GitCacheError::Validation(format!("invalid reservation marker {}", path.display()))
        })?;

    Ok(ReservationMarker {
        id,
        bytes,
        temp_dir: tmp_dir.join(id.to_string()),
        created_at_unix_millis: now_unix_millis(),
    })
}

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(path, bytes)?;
    Ok(())
}

fn uuid_from_marker_path(path: &Path) -> Option<Uuid> {
    let stem = path.file_stem()?.to_str()?;
    Uuid::parse_str(stem).ok()
}

fn uuid_from_dir_name(name: Option<&OsStr>) -> Option<Uuid> {
    Uuid::parse_str(name?.to_str()?).ok()
}

fn is_stale(path: &Path, older_than: Duration) -> Result<bool> {
    if older_than.is_zero() {
        return Ok(true);
    }

    let modified = path.metadata()?.modified()?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        >= older_than)
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
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
        let before = manager.repo_index().expect("index").repos[Path::new("repo.git")]
            .last_accessed_unix_millis;

        std::thread::sleep(Duration::from_millis(2));
        manager.note_repo_access("repo.git").expect("note");
        let unflushed = manager.repo_index().expect("index").repos[Path::new("repo.git")]
            .last_accessed_unix_millis;
        assert_eq!(unflushed, before);

        assert_eq!(manager.flush_repo_accesses().expect("flush"), 1);
        let flushed = manager.repo_index().expect("index").repos[Path::new("repo.git")]
            .last_accessed_unix_millis;
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
}
