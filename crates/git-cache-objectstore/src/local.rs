use crate::{validate_key, ObjectMeta, ObjectStore};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::io::AsyncWriteExt;

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

    async fn put(&self, key: &str, value: Bytes) -> Result<()> {
        let path = self.object_path(key)?;
        let parent = parent_dir(&path)?;
        fs::create_dir_all(parent).await?;

        let tmp_path = write_temp_file(parent, &path, value).await?;
        match fs::rename(&tmp_path, &path).await {
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

    async fn put_if_absent(&self, key: &str, value: Bytes) -> Result<bool> {
        let path = self.object_path(key)?;
        let parent = parent_dir(&path)?;
        fs::create_dir_all(parent).await?;

        let tmp_path = write_temp_file(parent, &path, value).await?;
        match fs::hard_link(&tmp_path, &path).await {
            Ok(()) => {
                let _ = fs::remove_file(&tmp_path).await;
                sync_directory(parent)?;
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

    async fn exists(&self, key: &str) -> Result<bool> {
        let path = self.object_path(key)?;
        match fs::metadata(path).await {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.object_path(key)?;
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
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
                        keys.push(rel.to_string_lossy().into_owned());
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
                }))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let dest = self.object_path(key)?;
        let parent = parent_dir(&dest)?;
        fs::create_dir_all(parent).await?;
        fs::copy(path, &dest).await?;
        sync_directory(parent)?;
        Ok(())
    }
}

fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent().ok_or_else(|| {
        GitCacheError::Validation(format!("object path `{}` has no parent", path.display()))
    })
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
