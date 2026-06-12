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

    /// Invalidate a cached repo. The repo directory is atomically renamed into
    /// the tmp area and the index entry removed while holding the state lock,
    /// so the repo is unavailable only for the duration of a rename; the slow
    /// recursive delete then runs outside any lock. Until that delete
    /// completes the moved directory still counts against the quota (usage is
    /// measured over the whole cache root, including tmp), so reservations
    /// cannot over-allocate. If the delete fails the directory is collected
    /// later by [`DiskManager::cleanup_stale_temps`].
    pub fn invalidate_repo(&self, repo_path: impl AsRef<Path>) -> Result<()> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let trash_dir = self.temp_dir_for(Uuid::new_v4());
        {
            let state = self
                .state
                .lock()
                .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
            if state.repo_locks.contains_key(&repo_path) {
                return Err(GitCacheError::Conflict(format!(
                    "repo `{}` is currently locked",
                    repo_path.display()
                )));
            }
            let repo_dir = self.repo_dir(&repo_path);
            if repo_dir.exists() {
                fs::rename(&repo_dir, &trash_dir)?;
            }
            let mut index = self.load_index()?;
            index.repos.remove(&repo_path);
            self.write_index(&index)?;
            if let Ok(mut pending) = self.pending_accesses.lock() {
                pending.remove(&repo_path);
            }
        }

        if trash_dir.exists() {
            fs::remove_dir_all(&trash_dir)?;
        }
        Ok(())
    }

    pub fn lock_repo(&self, repo_path: impl AsRef<Path>) -> Result<RepoLock> {
        self.ensure_layout()?;

        let repo_path = normalize_repo_path(&self.repos_dir(), repo_path.as_ref())?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| GitCacheError::Internal("disk manager mutex poisoned".into()))?;
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

            let Some(victim) = lru_evictable_repo(&index, &state.repo_locks) else {
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

fn lru_evictable_repo(index: &RepoIndex, locks: &HashMap<PathBuf, usize>) -> Option<PathBuf> {
    index
        .repos
        .values()
        .filter(|entry| !entry.protected && !locks.contains_key(&entry.path))
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
mod tests;
