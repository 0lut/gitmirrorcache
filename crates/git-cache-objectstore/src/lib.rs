#[cfg(all(feature = "s3", feature = "gcs"))]
compile_error!(
    "features `s3` and `gcs` are mutually exclusive: a deployment uses exactly one \
     durable object-store backend"
);

mod local;
mod manifests;

#[cfg(feature = "gcs")]
mod gcs;
#[cfg(feature = "s3")]
mod s3;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use git_cache_core::{GitCacheError, Result};
use std::path::{Component, Path};

pub use local::LocalObjectStore;
pub use manifests::{
    commit_manifest_key, generation_manifest_key, generation_manifest_prefix, pack_key,
    pack_prefix, read_commit_manifest, read_generation_manifest, read_json, read_ref_manifest,
    read_repo_generation_head, read_repo_generation_head_versioned, ref_manifest_key,
    repo_generation_head_key, write_commit_manifest, write_commit_manifest_if_absent_or_matches,
    write_generation_manifest, write_generation_manifest_if_absent_or_matches, write_json,
    write_json_if_absent, write_json_if_absent_or_matches, write_ref_manifest,
    write_repo_generation_head, write_repo_generation_head_if_version_matches, GenerationPublish,
    PublishManifests,
};

#[cfg(feature = "gcs")]
pub use gcs::GcsObjectStore;
#[cfg(feature = "s3")]
pub use s3::S3ObjectStore;

/// Opaque version token returned by `get_versioned` and consumed by
/// `put_if_version_matches`. Backends choose the representation (S3 uses
/// the object ETag, the local store uses a content digest); callers must
/// treat it as opaque and only pass it back to the same store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectVersion(String);

impl ObjectVersion {
    pub(crate) fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[cfg(any(feature = "s3", feature = "gcs"))]
    pub(crate) fn token(&self) -> &str {
        &self.0
    }
}

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

    /// Fetch an object together with an opaque version token for a later
    /// compare-and-swap via `put_if_version_matches`.
    async fn get_versioned(&self, key: &str) -> Result<Option<(Bytes, ObjectVersion)>>;

    /// Replace `key` only if the stored object still matches `version`.
    /// Returns `Ok(false)` when the object changed or no longer exists.
    async fn put_if_version_matches(
        &self,
        key: &str,
        value: Bytes,
        version: &ObjectVersion,
    ) -> Result<bool>;
    async fn exists(&self, key: &str) -> Result<bool>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list_prefix(&self, prefix: &str, max_keys: Option<usize>) -> Result<Vec<String>>;

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;

    async fn get_file(&self, key: &str, path: &Path) -> Result<bool> {
        let Some(data) = self.get(key).await? else {
            return Ok(false);
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, data).await?;
        Ok(true)
    }

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
