use crate::{validate_key, ObjectStore, ObjectVersion};
use bytes::Bytes;
use chrono::{DateTime, Duration, Utc};
use git_cache_core::{
    CommitManifest, GenerationId, GenerationManifest, GitCacheError, RefManifest,
    RepoGenerationHead, RepoKey, Result, SessionManifest, VerifiedGenerationManifest,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fmt::Debug;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseManifest {
    #[serde(default = "lease_schema_version")]
    pub schema_version: u32,
    pub repo: RepoKey,
    pub name: String,
    pub holder: String,
    #[serde(default)]
    pub token: String,
    pub acquired_at: DateTime<Utc>,
    #[serde(default)]
    pub renewed_at: Option<DateTime<Utc>>,
    pub expires_at: DateTime<Utc>,
    #[serde(default)]
    pub released_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub operation: Option<String>,
    #[serde(default)]
    pub expected_head: Option<GenerationId>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishManifests {
    pub commits: Vec<CommitManifest>,
    pub refs: Vec<RefManifest>,
    pub sessions: Vec<SessionManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingGenerationPublish {
    pub generation: GenerationManifest,
    pub manifests: PublishManifests,
    pub head: RepoGenerationHead,
    pub default_ref: Option<RefManifest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPublish {
    pub generation: GenerationManifest,
    pub verification: Option<VerifiedGenerationManifest>,
    pub manifests: PublishManifests,
}

impl GenerationPublish {
    pub fn new(generation: GenerationManifest) -> Self {
        Self {
            generation,
            verification: None,
            manifests: PublishManifests::default(),
        }
    }

    pub fn with_manifests(generation: GenerationManifest, manifests: PublishManifests) -> Self {
        Self {
            generation,
            verification: None,
            manifests,
        }
    }

    pub fn with_verification(mut self, verification: VerifiedGenerationManifest) -> Self {
        self.verification = Some(verification);
        self
    }

    pub async fn publish_bundle_bytes<S>(&self, store: &S, bundle: Bytes) -> Result<()>
    where
        S: ObjectStore + ?Sized,
    {
        validate_publish(self)?;
        put_bytes_if_absent_or_matches(store, &self.generation.bundle_key, bundle).await?;
        if let Some(verification) = &self.verification {
            write_verified_generation_manifest_if_absent_or_matches(store, verification).await?;
        }
        write_generation_manifest_if_absent_or_matches(store, &self.generation).await?;
        write_publish_manifests(store, &self.manifests).await
    }

    pub async fn publish_bundle_file<S>(&self, store: &S, path: impl AsRef<Path>) -> Result<()>
    where
        S: ObjectStore + ?Sized,
    {
        validate_publish(self)?;
        if store.exists(&self.generation.bundle_key).await? {
            return Err(GitCacheError::Conflict(format!(
                "bundle `{}` already exists",
                self.generation.bundle_key
            )));
        }
        store
            .put_file(&self.generation.bundle_key, path.as_ref())
            .await?;
        if let Some(verification) = &self.verification {
            write_verified_generation_manifest_if_absent_or_matches(store, verification).await?;
        }
        write_generation_manifest_if_absent_or_matches(store, &self.generation).await?;
        write_publish_manifests(store, &self.manifests).await
    }

    pub async fn publish_pending_bundle_file<S>(
        &self,
        store: &S,
        path: impl AsRef<Path>,
        head: RepoGenerationHead,
        default_ref: Option<RefManifest>,
    ) -> Result<()>
    where
        S: ObjectStore + ?Sized,
    {
        validate_publish(self)?;
        let pending = PendingGenerationPublish {
            generation: self.generation.clone(),
            manifests: self.manifests.clone(),
            head,
            default_ref,
        };
        validate_pending_publish(&pending)?;
        if store.exists(&self.generation.bundle_key).await? {
            return Err(GitCacheError::Conflict(format!(
                "bundle `{}` already exists",
                self.generation.bundle_key
            )));
        }
        store
            .put_file(&self.generation.bundle_key, path.as_ref())
            .await?;
        write_pending_generation_publish_if_absent_or_matches(store, &pending).await?;
        Ok(())
    }

    pub async fn publish_verified_metadata<S>(&self, store: &S) -> Result<()>
    where
        S: ObjectStore + ?Sized,
    {
        validate_publish(self)?;
        let verification = self.verification.as_ref().ok_or_else(|| {
            GitCacheError::Validation(format!(
                "generation `{}` is missing verified metadata",
                self.generation.generation
            ))
        })?;
        if !store.exists(&self.generation.bundle_key).await? {
            return Err(GitCacheError::NotFound(format!(
                "bundle `{}` not found",
                self.generation.bundle_key
            )));
        }
        write_verified_generation_manifest_if_absent_or_matches(store, verification).await?;
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

pub async fn read_json_with_version<S, T>(
    store: &S,
    key: &str,
) -> Result<Option<(T, ObjectVersion)>>
where
    S: ObjectStore + ?Sized,
    T: DeserializeOwned,
{
    let Some((value, version)) = store.get_with_version(key).await? else {
        return Ok(None);
    };
    Ok(Some((serde_json::from_slice(&value)?, version)))
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

pub async fn put_json_if_version_matches<S, T>(
    store: &S,
    key: &str,
    expected: &ObjectVersion,
    value: &T,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
    T: Serialize + ?Sized,
{
    store
        .put_if_version_matches(key, expected, json_bytes(value)?)
        .await
}

pub async fn compare_and_swap_json<S, T, F>(store: &S, key: &str, f: F) -> Result<bool>
where
    S: ObjectStore + ?Sized,
    T: Serialize + DeserializeOwned,
    F: FnOnce(Option<T>) -> Result<Option<T>>,
{
    match read_json_with_version::<_, T>(store, key).await? {
        Some((current, version)) => {
            let Some(next) = f(Some(current))? else {
                return store.delete_if_version_matches(key, &version).await;
            };
            put_json_if_version_matches(store, key, &version, &next).await
        }
        None => {
            let Some(next) = f(None)? else {
                return Ok(true);
            };
            write_json_if_absent(store, key, &next).await
        }
    }
}

pub fn generation_manifest_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!("repos/{repo}/generations/{generation}/manifest.json")
}

pub fn verified_generation_manifest_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!("repos/{repo}/generations/{generation}/verified.json")
}

pub fn pending_generation_publish_key(repo: &RepoKey, generation: GenerationId) -> String {
    format!("pending-generations/{repo}/{generation}.json")
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

pub fn session_manifest_key(repo: &RepoKey, session: git_cache_core::SessionId) -> String {
    format!("repos/{repo}/manifests/sessions/{session}.json")
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

pub async fn read_repo_generation_head_with_version<S>(
    store: &S,
    repo: &RepoKey,
) -> Result<Option<(RepoGenerationHead, ObjectVersion)>>
where
    S: ObjectStore + ?Sized,
{
    read_json_with_version(store, &repo_generation_head_key(repo)).await
}

pub async fn advance_generation_head<S>(
    store: &S,
    expected_current: Option<GenerationId>,
    expected_version: Option<&ObjectVersion>,
    new_head: &RepoGenerationHead,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let key = repo_generation_head_key(&new_head.repo);
    match read_json_with_version::<_, RepoGenerationHead>(store, &key).await? {
        Some((current, version)) => {
            if current.generation == new_head.generation {
                return Ok(true);
            }
            if Some(current.generation) != expected_current {
                return Ok(false);
            }
            if let Some(expected_version) = expected_version {
                if version.token != expected_version.token {
                    return Ok(false);
                }
            }
            put_json_if_version_matches(store, &key, &version, new_head).await
        }
        None => {
            if expected_current.is_some() || expected_version.is_some() {
                return Ok(false);
            }
            write_json_if_absent(store, &key, new_head).await
        }
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

pub async fn read_verified_generation_manifest<S>(
    store: &S,
    repo: &RepoKey,
    generation: GenerationId,
) -> Result<Option<VerifiedGenerationManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &verified_generation_manifest_key(repo, generation)).await
}

pub async fn write_verified_generation_manifest_if_absent_or_matches<S>(
    store: &S,
    manifest: &VerifiedGenerationManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    validate_verified_generation_manifest(manifest)?;
    write_json_if_absent_or_matches(
        store,
        &verified_generation_manifest_key(&manifest.repo, manifest.generation),
        manifest,
    )
    .await
}

pub async fn read_pending_generation_publish<S>(
    store: &S,
    repo: &RepoKey,
    generation: GenerationId,
) -> Result<Option<PendingGenerationPublish>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &pending_generation_publish_key(repo, generation)).await
}

pub async fn write_pending_generation_publish_if_absent_or_matches<S>(
    store: &S,
    pending: &PendingGenerationPublish,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    validate_pending_publish(pending)?;
    write_json_if_absent_or_matches(
        store,
        &pending_generation_publish_key(&pending.generation.repo, pending.generation.generation),
        pending,
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

pub async fn read_ref_manifest_with_version<S>(
    store: &S,
    repo: &RepoKey,
    ref_name: &str,
) -> Result<Option<(RefManifest, ObjectVersion)>>
where
    S: ObjectStore + ?Sized,
{
    read_json_with_version(store, &ref_manifest_key(repo, ref_name)?).await
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

pub async fn write_ref_manifest_if_version_matches<S>(
    store: &S,
    manifest: &RefManifest,
    expected: Option<&ObjectVersion>,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let key = ref_manifest_key(&manifest.repo, &manifest.ref_name)?;
    match expected {
        Some(version) => put_json_if_version_matches(store, &key, version, manifest).await,
        None => write_json_if_absent(store, &key, manifest).await,
    }
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

pub async fn read_session_manifest<S>(
    store: &S,
    repo: &RepoKey,
    session: git_cache_core::SessionId,
) -> Result<Option<SessionManifest>>
where
    S: ObjectStore + ?Sized,
{
    read_json(store, &session_manifest_key(repo, session)).await
}

pub async fn write_session_manifest<S>(store: &S, manifest: &SessionManifest) -> Result<()>
where
    S: ObjectStore + ?Sized,
{
    write_json(
        store,
        &session_manifest_key(&manifest.repo, manifest.id),
        manifest,
    )
    .await
}

pub async fn write_session_manifest_if_absent<S>(
    store: &S,
    manifest: &SessionManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent(
        store,
        &session_manifest_key(&manifest.repo, manifest.id),
        manifest,
    )
    .await
}

pub async fn write_session_manifest_if_absent_or_matches<S>(
    store: &S,
    manifest: &SessionManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    write_json_if_absent_or_matches(
        store,
        &session_manifest_key(&manifest.repo, manifest.id),
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
    let token = git_cache_core::GenerationId::new().to_string();
    acquire_lease_with_token(store, repo, name, holder, token, acquired_at, ttl).await
}

pub async fn acquire_lease_with_token<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
    holder: impl Into<String>,
    token: impl Into<String>,
    acquired_at: DateTime<Utc>,
    ttl: Duration,
) -> Result<Option<LeaseManifest>>
where
    S: ObjectStore + ?Sized,
{
    let lease = LeaseManifest {
        schema_version: lease_schema_version(),
        repo: repo.clone(),
        name: name.to_string(),
        holder: holder.into(),
        token: token.into(),
        acquired_at,
        renewed_at: Some(acquired_at),
        expires_at: acquired_at + ttl,
        released_at: None,
        operation: None,
        expected_head: None,
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

pub async fn read_lease_with_version<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
) -> Result<Option<(LeaseManifest, ObjectVersion)>>
where
    S: ObjectStore + ?Sized,
{
    read_json_with_version(store, &lease_key(repo, name)?).await
}

pub async fn renew_lease_if_token_matches<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
    token: &str,
    renewed_at: DateTime<Utc>,
    ttl: Duration,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let key = lease_key(repo, name)?;
    let Some((mut lease, version)) =
        read_json_with_version::<_, LeaseManifest>(store, &key).await?
    else {
        return Ok(false);
    };
    if lease.token != token || lease.released_at.is_some() {
        return Ok(false);
    }
    lease.renewed_at = Some(renewed_at);
    lease.expires_at = renewed_at + ttl;
    put_json_if_version_matches(store, &key, &version, &lease).await
}

pub async fn release_lease_if_token_matches<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
    token: &str,
    released_at: DateTime<Utc>,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    let key = lease_key(repo, name)?;
    loop {
        let Some((mut lease, version)) =
            read_json_with_version::<_, LeaseManifest>(store, &key).await?
        else {
            return Ok(false);
        };
        if lease.token != token {
            return Ok(false);
        }
        if lease.released_at.is_some() {
            return Ok(true);
        }
        lease.released_at = Some(released_at);
        if put_json_if_version_matches(store, &key, &version, &lease).await? {
            return Ok(true);
        }
    }
}

pub async fn steal_expired_lease_if_version_matches<S>(
    store: &S,
    repo: &RepoKey,
    name: &str,
    expected: &ObjectVersion,
    mut lease: LeaseManifest,
) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    lease.schema_version = lease_schema_version();
    put_json_if_version_matches(store, &lease_key(repo, name)?, expected, &lease).await
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

    for manifest in &manifests.sessions {
        write_session_manifest_if_absent_or_matches(store, manifest).await?;
    }

    Ok(())
}

async fn put_bytes_if_absent_or_matches<S>(store: &S, key: &str, value: Bytes) -> Result<bool>
where
    S: ObjectStore + ?Sized,
{
    if store.put_if_absent(key, value.clone()).await? {
        return Ok(true);
    }

    let existing = store
        .get(key)
        .await?
        .ok_or_else(|| GitCacheError::Conflict(format!("object `{key}` disappeared")))?;
    if existing == value {
        Ok(false)
    } else {
        Err(GitCacheError::Conflict(format!(
            "object `{key}` already contains different bytes"
        )))
    }
}

fn json_bytes<T>(value: &T) -> Result<Bytes>
where
    T: Serialize + ?Sized,
{
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(Bytes::from(bytes))
}

fn lease_schema_version() -> u32 {
    1
}

fn validate_publish(publish: &GenerationPublish) -> Result<()> {
    validate_generation_manifest(&publish.generation)?;
    let repo = &publish.generation.repo;
    let generation = publish.generation.generation;

    if let Some(verification) = &publish.verification {
        validate_verified_generation_manifest(verification)?;
        if verification.repo != *repo
            || verification.generation != generation
            || verification.bundle_key != publish.generation.bundle_key
            || verification.parent_generation != publish.generation.parent_generation
        {
            return Err(GitCacheError::Validation(format!(
                "verified generation manifest does not match generation `{generation}`"
            )));
        }
    }

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

    for manifest in &publish.manifests.sessions {
        if manifest.repo != *repo {
            return Err(GitCacheError::Validation(format!(
                "session manifest `{}` does not match repo `{repo}`",
                manifest.id
            )));
        }
    }

    Ok(())
}

fn validate_pending_publish(pending: &PendingGenerationPublish) -> Result<()> {
    validate_generation_manifest(&pending.generation)?;
    let repo = &pending.generation.repo;
    let generation = pending.generation.generation;

    if pending.head.repo != *repo || pending.head.generation != generation {
        return Err(GitCacheError::Validation(format!(
            "pending generation head does not match generation `{generation}`"
        )));
    }

    let publish =
        GenerationPublish::with_manifests(pending.generation.clone(), pending.manifests.clone());
    validate_publish(&publish)?;

    if let Some(default_ref) = &pending.default_ref {
        if default_ref.repo != *repo
            || default_ref.generation != generation
            || default_ref.ref_name != "HEAD"
        {
            return Err(GitCacheError::Validation(format!(
                "pending default ref does not match generation `{generation}`"
            )));
        }
    }

    Ok(())
}

fn validate_generation_manifest(manifest: &GenerationManifest) -> Result<()> {
    validate_key(&manifest.bundle_key)?;
    Ok(())
}

fn validate_verified_generation_manifest(manifest: &VerifiedGenerationManifest) -> Result<()> {
    if manifest.schema_version != 2 {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has unsupported schema version {}",
            manifest.generation, manifest.schema_version
        )));
    }
    if manifest.bundle_len == 0 {
        return Err(GitCacheError::Validation(format!(
            "verified generation `{}` has empty bundle",
            manifest.generation
        )));
    }
    validate_key(&manifest.bundle_key)?;
    validate_sha256(&manifest.bundle_sha256)?;
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
