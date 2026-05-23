mod local;
mod manifests;

#[cfg(feature = "s3")]
mod s3;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::{Component, Path};

pub use local::LocalObjectStore;
pub use manifests::{
    acquire_lease, commit_manifest_key, generation_manifest_key, lease_key, read_commit_manifest,
    read_generation_manifest, read_json, read_lease, read_ref_manifest, read_repo_generation_head,
    read_session_manifest, ref_manifest_key, repo_generation_head_key, session_manifest_key,
    write_commit_manifest, write_commit_manifest_if_absent,
    write_commit_manifest_if_absent_or_matches, write_generation_manifest,
    write_generation_manifest_if_absent, write_generation_manifest_if_absent_or_matches,
    write_json, write_json_if_absent, write_json_if_absent_or_matches, write_ref_manifest,
    write_ref_manifest_if_absent, write_ref_manifest_if_absent_or_matches,
    write_repo_generation_head, write_session_manifest, write_session_manifest_if_absent,
    write_session_manifest_if_absent_or_matches, GenerationPublish, LeaseManifest,
    PublishManifests,
};

#[cfg(feature = "s3")]
pub use s3::S3ObjectStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub len: u64,
    pub updated_at: Option<DateTime<Utc>>,
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Bytes>>;
    async fn put(&self, key: &str, value: Bytes) -> Result<()>;
    async fn put_if_absent(&self, key: &str, value: Bytes) -> Result<bool>;
    async fn exists(&self, key: &str) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list_prefix(&self, prefix: &str, max_keys: Option<usize>) -> Result<Vec<String>>;

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;

    async fn put_file(&self, key: &str, path: &Path) -> Result<()> {
        let data = tokio::fs::read(path).await?;
        self.put(key, Bytes::from(data)).await
    }
}

pub(crate) fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('/') || key.ends_with('/') || key.contains('\\') {
        return Err(GitCacheError::Validation(format!(
            "object key `{key}` must be a relative object path"
        )));
    }

    if key.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "object key `{key}` contains a NUL byte"
        )));
    }

    if key
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(GitCacheError::Validation(format!(
            "object key `{key}` contains an unsafe path segment"
        )));
    }

    for component in Path::new(key).components() {
        match component {
            Component::Normal(_) => {}
            _ => {
                return Err(GitCacheError::Validation(format!(
                    "object key `{key}` contains an unsafe path component"
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests;
