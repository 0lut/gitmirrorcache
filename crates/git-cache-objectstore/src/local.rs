use crate::{validate_key, ObjectMeta, ObjectStore, ObjectVersion};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;

const LOCAL_LOCK_RETRY_COUNT: usize = 200;
const LOCAL_LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn object_path(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self.root.join(key))
    }

    fn version_path(&self, key: &str) -> Result<PathBuf> {
        let object_path = self.object_path(key)?;
        let file_name = object_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("object");
        Ok(object_path.with_file_name(format!(".{file_name}.version")))
    }

    fn lock_path(&self, key: &str) -> Result<PathBuf> {
        validate_key(key)?;
        Ok(self
            .root
            .join(".locks")
            .join(format!("{}.lock", hex_key(key))))
    }

    async fn read_version(&self, key: &str, path: &Path) -> Result<Option<ObjectVersion>> {
        let updated_at = metadata_updated_at(path).await?;
        let version_path = self.version_path(key)?;
        match fs::read_to_string(&version_path).await {
            Ok(token) => Ok(Some(ObjectVersion {
                token: token.trim().to_string(),
                updated_at,
            })),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let metadata = match fs::metadata(path).await {
                    Ok(metadata) => metadata,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
                Ok(Some(ObjectVersion {
                    token: legacy_version_token(&metadata),
                    updated_at,
                }))
            }
            Err(err) => Err(err.into()),
        }
    }

    async fn write_version(&self, key: &str) -> Result<ObjectVersion> {
        let path = self.object_path(key)?;
        let version_path = self.version_path(key)?;
        let parent = parent_dir(&version_path)?;
        fs::create_dir_all(parent).await?;
        let token = new_local_version_token();
        let tmp_path = write_temp_file(parent, &version_path, Bytes::from(token.clone())).await?;
        match fs::rename(&tmp_path, &version_path).await {
            Ok(()) => {
                sync_directory(parent)?;
                Ok(ObjectVersion {
                    token,
                    updated_at: metadata_updated_at(&path).await?,
                })
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path).await;
                Err(err.into())
            }
        }
    }

    async fn put_inner(&self, key: &str, value: Bytes) -> Result<ObjectVersion> {
        let path = self.object_path(key)?;
        let parent = parent_dir(&path)?;
        fs::create_dir_all(parent).await?;

        let tmp_path = write_temp_file(parent, &path, value).await?;
        // Write version token BEFORE renaming data so that a crash between
        // the version write and the data rename leaves a new token guarding
        // old bytes.  Any CAS holder with the previous token will fail,
        // preventing writes against data they never read.
        let version = self.write_version(key).await?;
        match fs::rename(&tmp_path, &path).await {
            Ok(()) => {
                sync_directory(parent)?;
                Ok(version)
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path).await;
                Err(err.into())
            }
        }
    }

    async fn acquire_key_lock(&self, key: &str) -> Result<LocalObjectLock> {
        let lock_path = self.lock_path(key)?;
        let parent = parent_dir(&lock_path)?;
        fs::create_dir_all(parent).await?;
        for _ in 0..LOCAL_LOCK_RETRY_COUNT {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
                .await
            {
                Ok(mut file) => {
                    let created_at_ms = now_unix_millis();
                    file.write_all(
                        format!(
                            "pid={}\ncreated_at_unix_ms={created_at_ms}\n",
                            std::process::id()
                        )
                        .as_bytes(),
                    )
                    .await?;
                    file.sync_all().await?;
                    return Ok(LocalObjectLock { path: lock_path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.recover_stale_lock(&lock_path).await?;
                    tokio::time::sleep(LOCAL_LOCK_RETRY_DELAY).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Err(GitCacheError::Conflict(format!(
            "timed out acquiring local object lock for `{key}`"
        )))
    }

    async fn recover_stale_lock(&self, lock_path: &Path) -> Result<()> {
        let contents = match fs::read_to_string(lock_path).await {
            Ok(contents) => contents,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        if lock_owner_pid(&contents).is_none_or(process_is_alive) {
            return Ok(());
        }
        match fs::remove_file(lock_path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    async fn get(&self, key: &str) -> Result<Option<Bytes>> {
        let path = self.object_path(key)?;
        match fs::read(path).await {
            Ok(value) => Ok(Some(Bytes::from(value))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn get_with_version(&self, key: &str) -> Result<Option<(Bytes, ObjectVersion)>> {
        let _lock = self.acquire_key_lock(key).await?;
        let path = self.object_path(key)?;
        let value = match fs::read(&path).await {
            Ok(value) => Bytes::from(value),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let version = self
            .read_version(key, &path)
            .await?
            .ok_or_else(|| GitCacheError::Conflict(format!("object `{key}` disappeared")))?;
        Ok(Some((value, version)))
    }

    async fn put(&self, key: &str, value: Bytes) -> Result<()> {
        let _lock = self.acquire_key_lock(key).await?;
        self.put_inner(key, value).await?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, value: Bytes) -> Result<bool> {
        let _lock = self.acquire_key_lock(key).await?;
        let path = self.object_path(key)?;
        let parent = parent_dir(&path)?;
        fs::create_dir_all(parent).await?;

        let tmp_path = write_temp_file(parent, &path, value).await?;
        match fs::hard_link(&tmp_path, &path).await {
            Ok(()) => {
                let _ = fs::remove_file(&tmp_path).await;
                sync_directory(parent)?;
                self.write_version(key).await?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp_path).await;
                Ok(false)
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path).await;
                Err(err.into())
            }
        }
    }

    async fn put_if_version_matches(
        &self,
        key: &str,
        expected: &ObjectVersion,
        value: Bytes,
    ) -> Result<bool> {
        let _lock = self.acquire_key_lock(key).await?;
        let path = self.object_path(key)?;
        let Some(current) = self.read_version(key, &path).await? else {
            return Ok(false);
        };
        if current.token != expected.token {
            return Ok(false);
        }
        self.put_inner(key, value).await?;
        Ok(true)
    }

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.object_path(key)?;
        match fs::metadata(path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let _lock = self.acquire_key_lock(key).await?;
        let path = self.object_path(key)?;
        if let Err(err) = fs::remove_file(&path).await {
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err.into());
            }
        }
        let _ = fs::remove_file(self.version_path(key)?).await;
        Ok(())
    }

    async fn delete_if_version_matches(&self, key: &str, expected: &ObjectVersion) -> Result<bool> {
        let _lock = self.acquire_key_lock(key).await?;
        let path = self.object_path(key)?;
        let Some(current) = self.read_version(key, &path).await? else {
            return Ok(false);
        };
        if current.token != expected.token {
            return Ok(false);
        }
        match fs::remove_file(&path).await {
            Ok(()) => {
                let _ = fs::remove_file(self.version_path(key)?).await;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    async fn list_prefix(&self, prefix: &str, max_keys: Option<usize>) -> Result<Vec<String>> {
        let base = self.root.join(prefix);
        let mut keys = Vec::new();

        if !matches!(fs::metadata(&base).await, Ok(m) if m.is_dir()) {
            return Ok(keys);
        }

        let mut stack = vec![base.clone()];
        'outer: while let Some(dir) = stack.pop() {
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    if let Ok(rel) = path.strip_prefix(&self.root) {
                        let key = rel.to_string_lossy().into_owned();
                        if key.starts_with(".locks/")
                            || key.rsplit('/').next().is_some_and(|name| {
                                name.starts_with('.') && name.ends_with(".version")
                            })
                        {
                            continue;
                        }
                        keys.push(key);
                        if let Some(limit) = max_keys {
                            if keys.len() >= limit {
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        keys.sort();
        Ok(keys)
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let path = self.object_path(key)?;
        match fs::metadata(&path).await {
            Ok(metadata) => {
                let updated_at = metadata.modified().ok().and_then(|t| {
                    t.duration_since(UNIX_EPOCH).ok().and_then(|d| {
                        DateTime::<Utc>::from_timestamp(d.as_secs() as i64, d.subsec_nanos())
                    })
                });
                Ok(Some(ObjectMeta {
                    key: key.to_string(),
                    len: metadata.len(),
                    updated_at,
                    version: self.read_version(key, &path).await?,
                }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn get_file(&self, key: &str, dest: &Path) -> Result<bool> {
        let src = self.object_path(key)?;
        if !matches!(fs::metadata(&src).await, Ok(metadata) if metadata.is_file()) {
            return Ok(false);
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(src, dest).await?;
        Ok(true)
    }

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let _lock = self.acquire_key_lock(key).await?;
        let dest = self.object_path(key)?;
        let parent = parent_dir(&dest)?;
        fs::create_dir_all(parent).await?;

        let tmp_path = allocate_temp_path(parent, &dest)?;
        fs::copy(path, &tmp_path).await?;
        let tmp_file = fs::File::open(&tmp_path).await?;
        tmp_file.sync_all().await?;
        drop(tmp_file);
        // Write version before data rename for crash consistency (see put_inner).
        self.write_version(key).await?;
        match fs::rename(&tmp_path, &dest).await {
            Ok(()) => {
                sync_directory(parent)?;
                Ok(())
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path).await;
                Err(err.into())
            }
        }
    }
}

fn allocate_temp_path(parent: &Path, final_path: &Path) -> Result<PathBuf> {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok(parent.join(format!(".{file_name}.{pid}.{nanos}.tmp")))
}

fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        GitCacheError::Validation(format!("object path `{}` has no parent", path.display()))
    })
}

struct LocalObjectLock {
    path: PathBuf,
}

impl Drop for LocalObjectLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn metadata_updated_at(path: &Path) -> Result<Option<DateTime<Utc>>> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.modified().ok().and_then(system_time_to_utc)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn legacy_version_token(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("legacy:{}:{modified}", metadata.len())
}

fn new_local_version_token() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{pid}-{nanos}")
}

fn system_time_to_utc(time: SystemTime) -> Option<DateTime<Utc>> {
    time.duration_since(UNIX_EPOCH).ok().and_then(|duration| {
        DateTime::<Utc>::from_timestamp(duration.as_secs() as i64, duration.subsec_nanos())
    })
}

fn now_unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn lock_owner_pid(contents: &str) -> Option<u32> {
    contents.lines().find_map(|line| {
        line.strip_prefix("pid=")
            .and_then(|pid| pid.trim().parse().ok())
    })
}

fn process_is_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    let proc_root = std::path::Path::new("/proc");
    if !proc_root.exists() {
        return true;
    }
    proc_root.join(pid.to_string()).exists()
}

fn hex_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len() * 2);
    for byte in key.as_bytes() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

async fn write_temp_file(parent: &Path, final_path: &Path, value: Bytes) -> Result<PathBuf> {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let pid = std::process::id();

    for attempt in 0..32 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let tmp_path = parent.join(format!(".{file_name}.{pid}.{nanos}.{attempt}.tmp"));

        let mut file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        };

        file.write_all(&value).await?;
        file.sync_all().await?;
        drop(file);
        return Ok(tmp_path);
    }

    Err(GitCacheError::Conflict(format!(
        "could not allocate temp file below `{}`",
        parent.display()
    )))
}

fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}
