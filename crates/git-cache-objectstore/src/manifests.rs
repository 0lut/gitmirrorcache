use crate::{validate_key, ObjectStore, ObjectVersion};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use git_cache_core::{
    CommitManifest, GenerationId, GenerationManifest, GitCacheError, RefManifest,
    RepoGenerationHead, RepoKey, Result,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt::Debug;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseManifest {
    pub repo: RepoKey,
    pub name: String,
    pub holder: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishManifests {
    pub commits: Vec<CommitManifest>,
    pub refs: Vec<RefManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPublish {
    pub generation: GenerationManifest,
    pub manifests: PublishManifests,
}

impl GenerationPublish {
    pub fn new(generation: GenerationManifest) -> Self {
        Self {
            generation,
            manifests: PublishManifests::default(),
        }
    }

    pub fn with_manifests(generation: GenerationManifest, manifests: PublishManifests) -> Self {
        Self {
            generation,
            manifests,
        }
    }

    /// Publish a generation whose packs are content-addressed objects. Each
    /// entry in `local_packs` maps a pack key from the manifest to a local
    /// file containing the pack bytes; packs already present in the store
    /// are skipped (identical content shares an address).
    pub async fn publish_pack_files<S>(
        &self,
        store: &S,
        local_packs: &[(String, std::path::PathBuf)],
    ) -> Result<()>
    where
        S: ObjectStore + ?Sized,
    {
        validate_publish(self)?;
        for (key, path) in local_packs {
            if !self.generation.packs.iter().any(|pack| pack.key == *key) {
                return Err(GitCacheError::Validation(format!(
                    "pack `{key}` is not referenced by generation `{}`",
                    self.generation.generation
                )));
            }
            if !store.exists(key).await? {
                store.put_file(key, path.as_path()).await?;
            }
        }
        write_generation_manifest_if_absent_or_matches(store, &self.generation).await?;
        write_publish_manifests(store, &self.manifests).await
    }
}

pub async fn read_json<S, T>(store: &S, key: &str) -> Result<Option<T>>
where
    S: ObjectStore + ?Sized,
    T: DeserializeOwned,
{
    let Some(value) = store.get(key).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&value)?))
}

pub async fn write_json<S, T>(store: &S, key: &str, value: &T) -> Result<()>
where
    S: ObjectStore + ?Sized,
    T: Serialize + ?Sized,
{
    store.put(key, json_bytes(value)?).await
}

pub async fn write_json_if_absent<S, T>(store: &S, key: &str, value: &T) -> Result<bool>
where
    S: ObjectStore + ?Sized,
    T: Serialize + ?Sized,
{
    store.put_if_absent(key, json_bytes(value)?).await
}

pub async fn write_json_if_absent_or_matches<S, T>(store: &S, key: &str, value: &T) -> Result<bool>
where
    S: ObjectStore + ?Sized,
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    if write_json_if_absent(store, key, value).await? {
        return Ok(true);
    }

    let existing = read_json::<_, T>(store, key).await?.ok_or_else(|| {
        GitCacheError::Conflict(format!("object `{key}` existed during conditional write"))
    })?;
    if existing == *value {
        Ok(false)
    } else {
        Err(GitCacheError::Conflict(format!(
            "object `{key}` already contains a different manifest"
        )))
    }
}

pub fn generation_manifest_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!("repos/{repo}/generations/{generation}/manifest.json")
}

pub fn generation_manifest_prefix(repo: &RepoKey) -> String {
    format!("repos/{repo}/generations/")
}

pub fn pack_key(repo: &RepoKey, sha256: &str) -> Result<String> {
    validate_sha256(sha256)?;
    Ok(format!("repos/{repo}/packs/pack-{sha256}.pack"))
}

pub fn commit_manifest_key(repo: &RepoKey, commit: &git_cache_core::CommitSha) -> String {
    let sha = commit.as_str();
    format!("repos/{repo}/manifests/commits/{}/{}.json", &sha[..2], sha)
}

pub fn ref_manifest_key(repo: &RepoKey, ref_name: &str) -> Result<String> {
    validate_ref_name(ref_name)?;
    if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
        Ok(format!(
            "repos/{repo}/manifests/refs/heads/{}.json",
            encode_component(branch)
        ))
    } else {
        Ok(format!(
            "repos/{repo}/manifests/refs/{}.json",
            encode_component(ref_name)
        ))
    }
}

pub fn repo_generation_head_key(repo: &RepoKey) -> String {
    format!("repos/{repo}/manifests/generation-head.json")
}

pub fn lease_key(repo: &RepoKey, name: &str) -> Result<String> {
    validate_name(name, "lease")?;
    Ok(format!(
        "repos/{repo}/leases/{}.json",
        encode_component(name)
    ))
}

fn ref_observation_manifest_key(manifest: &RefManifest) -> Result<String> {
    validate_ref_name(&manifest.ref_name)?;
    Ok(format!(
        "repos/{}/manifests/ref-updates/{}/{}.json",
        manifest.repo,
        encode_component(&manifest.ref_name),
        manifest.generation
    ))
}

pub async fn read_generation_manifest<S>(
    store: &S,
    repo: &RepoKey,
    generation: GenerationId,
) -> Result<Option<GenerationManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &generation_manifest_key(repo, generation)).await
}

pub async fn read_repo_generation_head<S>(
    store: &S,
    repo: &RepoKey,
) -> Result<Option<RepoGenerationHead>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &repo_generation_head_key(repo)).await
}

pub async fn write_repo_generation_head<S>(store: &S, head: &RepoGenerationHead) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    write_json(store, &repo_generation_head_key(&head.repo), head).await
}

pub async fn read_repo_generation_head_versioned<S>(
    store: &S,
    repo: &RepoKey,
) -> Result<Option<(RepoGenerationHead, ObjectVersion)>>
where
    S: ObjectStore + ?Sized,
{
    let Some((value, version)) = store
        .get_versioned(&repo_generation_head_key(repo))
        .await?
    else {
        return Ok(None);
    };
    Ok(Some((serde_json::from_slice(&value)?, version)))
}

/// Compare-and-swap write of the generation head pointer. `version` is the
/// token from `read_repo_generation_head_versioned`; `None` means the head
/// is expected to be absent (first write). Returns `Ok(false)` when the
/// stored head no longer matches the expectation.
pub async fn write_repo_generation_head_if_version_matches<S>(
    store: &S,
    head: &RepoGenerationHead,
    version: Option<&ObjectVersion>,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let key = repo_generation_head_key(&head.repo);
    let bytes = json_bytes(head)?;
    match version {
        Some(version) => store.put_if_version_matches(&key, bytes, version).await,
        None => store.put_if_absent(&key, bytes).await,
    }
}

pub async fn write_generation_manifest<S>(store: &S, manifest: &GenerationManifest) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    validate_generation_manifest(manifest)?;
    write_json(
        store,
        &generation_manifest_key(&manifest.repo, manifest.generation),
        manifest,
    )
    .await
}

pub async fn write_generation_manifest_if_absent<S>(
    store: &S,
    manifest: &GenerationManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    validate_generation_manifest(manifest)?;
    write_json_if_absent(
        store,
        &generation_manifest_key(&manifest.repo, manifest.generation),
        manifest,
    )
    .await
}

pub async fn write_generation_manifest_if_absent_or_matches<S>(
    store: &S,
    manifest: &GenerationManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    validate_generation_manifest(manifest)?;
    write_json_if_absent_or_matches(
        store,
        &generation_manifest_key(&manifest.repo, manifest.generation),
        manifest,
    )
    .await
}

pub async fn read_commit_manifest<S>(
    store: &S,
    repo: &RepoKey,
    commit: &git_cache_core::CommitSha,
) -> Result<Option<CommitManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &commit_manifest_key(repo, commit)).await
}

pub async fn write_commit_manifest<S>(store: &S, manifest: &CommitManifest) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    write_json(
        store,
        &commit_manifest_key(&manifest.repo, &manifest.commit),
        manifest,
    )
    .await
}

pub async fn write_commit_manifest_if_absent<S>(
    store: &S,
    manifest: &CommitManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent(
        store,
        &commit_manifest_key(&manifest.repo, &manifest.commit),
        manifest,
    )
    .await
}

pub async fn write_commit_manifest_if_absent_or_matches<S>(
    store: &S,
    manifest: &CommitManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent_or_matches(
        store,
        &commit_manifest_key(&manifest.repo, &manifest.commit),
        manifest,
    )
    .await
}

pub async fn read_ref_manifest<S>(
    store: &S,
    repo: &RepoKey,
    ref_name: &str,
) -> Result<Option<RefManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &ref_manifest_key(repo, ref_name)?).await
}

pub async fn write_ref_manifest<S>(store: &S, manifest: &RefManifest) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    write_json(
        store,
        &ref_manifest_key(&manifest.repo, &manifest.ref_name)?,
        manifest,
    )
    .await
}

pub async fn write_ref_manifest_if_absent<S>(store: &S, manifest: &RefManifest) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent(
        store,
        &ref_manifest_key(&manifest.repo, &manifest.ref_name)?,
        manifest,
    )
    .await
}

pub async fn write_ref_manifest_if_absent_or_matches<S>(
    store: &S,
    manifest: &RefManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent_or_matches(
        store,
        &ref_manifest_key(&manifest.repo, &manifest.ref_name)?,
        manifest,
    )
    .await
}

pub async fn acquire_lease<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
    holder: impl Into<String>,
    acquired_at: DateTime<Utc>,
    ttl: Duration,
) -> Result<Option<LeaseManifest>>
where
    S: ObjectStore + ?Sized,
{
    let lease = LeaseManifest {
        repo: repo.clone(),
        name: name.to_string(),
        holder: holder.into(),
        acquired_at,
        expires_at: acquired_at + ttl,
    };
    let key = lease_key(repo, name)?;
    if write_json_if_absent(store, &key, &lease).await? {
        Ok(Some(lease))
    } else {
        Ok(None)
    }
}

pub async fn read_lease<S>(store: &S, repo: &RepoKey, name: &str) -> Result<Option<LeaseManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &lease_key(repo, name)?).await
}

async fn write_publish_manifests<S>(store: &S, manifests: &PublishManifests) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    for manifest in &manifests.commits {
        write_commit_manifest_if_absent_or_matches(store, manifest).await?;
    }

    for manifest in &manifests.refs {
        write_json_if_absent_or_matches(store, &ref_observation_manifest_key(manifest)?, manifest)
            .await?;
        write_ref_manifest(store, manifest).await?;
    }

    Ok(())
}

fn json_bytes<T>(value: &T) -> Result<Bytes>
where
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

fn validate_publish(publish: &GenerationPublish) -> Result<()> {
    validate_generation_manifest(&publish.generation)?;
    let repo = &publish.generation.repo;
    let generation = publish.generation.generation;

    for manifest in &publish.manifests.commits {
        if manifest.repo != *repo || manifest.generation != generation {
            return Err(GitCacheError::Validation(format!(
                "commit manifest for `{}` does not match generation `{generation}`",
                manifest.commit
            )));
        }
    }

    for manifest in &publish.manifests.refs {
        validate_ref_name(&manifest.ref_name)?;
        if manifest.repo != *repo || manifest.generation != generation {
            return Err(GitCacheError::Validation(format!(
                "ref manifest for `{}` does not match generation `{generation}`",
                manifest.ref_name
            )));
        }
    }

    Ok(())
}

fn validate_generation_manifest(manifest: &GenerationManifest) -> Result<()> {
    for pack in &manifest.packs {
        validate_key(&pack.key)?;
        validate_sha256(&pack.sha256)?;
        if pack.len == 0 {
            return Err(GitCacheError::Validation(format!(
                "generation `{}` references empty pack `{}`",
                manifest.generation, pack.key
            )));
        }
        let expected = pack_key(&manifest.repo, &pack.sha256)?;
        if pack.key != expected {
            return Err(GitCacheError::Validation(format!(
                "pack key `{}` does not match content address `{expected}`",
                pack.key
            )));
        }
    }
    for ref_name in manifest.refs.keys() {
        validate_ref_name(ref_name)?;
    }
    if let Some(head_ref) = &manifest.head_ref {
        validate_ref_name(head_ref)?;
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitCacheError::Validation(format!(
            "invalid sha256 digest `{value}`"
        )));
    }
    Ok(())
}

fn validate_ref_name(ref_name: &str) -> Result<()> {
    validate_name(ref_name, "ref")
}

fn validate_name(value: &str, kind: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.contains("//")
    {
        return Err(GitCacheError::Validation(format!(
            "{kind} name `{value}` is not a safe object-store name"
        )));
    }

    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." || segment.ends_with(".lock") {
            return Err(GitCacheError::Validation(format!(
                "{kind} name `{value}` contains an unsafe segment"
            )));
        }
    }

    Ok(())
}

fn encode_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());

    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }

    encoded
}
