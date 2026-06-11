//! In-process implementations of local, read-only Git operations using
//! gitoxide (`gix`). These run inside the cache's bare repositories and never
//! touch the network. All functions are synchronous and intended to be called
//! through `tokio::task::spawn_blocking`.
//!
//! Inputs are validated by the `Git` wrapper before reaching this module, and
//! no subprocess is spawned, so there is no argv injection surface here.

use git_cache_core::{CommitSha, GitCacheError, Result};
use std::collections::HashMap;
use std::path::Path;

fn open(repo_dir: &Path) -> Result<gix::Repository> {
    gix::open_opts(repo_dir, gix::open::Options::isolated()).map_err(|err| {
        GitCacheError::Validation(format!(
            "gitoxide failed to open repository {}: {err}",
            repo_dir.display()
        ))
    })
}

fn parse_object_id(value: &str) -> Result<gix::ObjectId> {
    gix::ObjectId::from_hex(value.as_bytes())
        .map_err(|err| GitCacheError::Validation(format!("invalid object id {value:?}: {err}")))
}

/// Equivalent of `git rev-parse --verify --end-of-options {rev}`.
pub(crate) fn rev_parse(repo_dir: &Path, rev: &str) -> Result<String> {
    let repo = open(repo_dir)?;
    let id = repo.rev_parse_single(rev).map_err(|err| {
        GitCacheError::Validation(format!("gitoxide rev-parse failed for {rev:?}: {err}"))
    })?;
    Ok(id.detach().to_string())
}

/// Equivalent of `git for-each-ref --format='%(refname) %(objectname)' -- {prefix}`.
pub(crate) fn for_each_ref(repo_dir: &Path, ref_prefix: &str) -> Result<Vec<(String, CommitSha)>> {
    let repo = open(repo_dir)?;
    let references = repo
        .references()
        .map_err(|err| GitCacheError::Validation(format!("gitoxide failed to read refs: {err}")))?;
    let iter = references.prefixed(ref_prefix).map_err(|err| {
        GitCacheError::Validation(format!(
            "gitoxide failed to iterate refs with prefix {ref_prefix:?}: {err}"
        ))
    })?;

    let mut refs = Vec::new();
    for reference in iter {
        let reference = reference.map_err(|err| {
            GitCacheError::Validation(format!("gitoxide failed to read ref entry: {err}"))
        })?;
        let name = reference.name().as_bstr().to_string();
        let id = resolve_ref_object_id(reference)?;
        refs.push((name, CommitSha::parse(id.to_string().as_str())?));
    }
    Ok(refs)
}

/// Equivalent of `git for-each-ref --format='%(objectname)' -- {prefix}`.
pub(crate) fn for_each_ref_commits(repo_dir: &Path, ref_prefix: &str) -> Result<Vec<CommitSha>> {
    Ok(for_each_ref(repo_dir, ref_prefix)?
        .into_iter()
        .map(|(_, commit)| commit)
        .collect())
}

fn resolve_ref_object_id(reference: gix::Reference<'_>) -> Result<gix::ObjectId> {
    let mut current = reference;
    // Symbolic refs are followed to their direct target without peeling
    // annotated tags, matching `%(objectname)` semantics.
    for _ in 0..16 {
        if let Some(id) = current.try_id() {
            return Ok(id.detach());
        }
        match current.follow() {
            Some(next) => {
                current = next.map_err(|err| {
                    GitCacheError::Validation(format!(
                        "gitoxide failed to follow symbolic ref: {err}"
                    ))
                })?;
            }
            None => break,
        }
    }
    Err(GitCacheError::Validation(
        "gitoxide could not resolve ref to an object id".into(),
    ))
}

/// Equivalent of `git rev-list --max-count=1 {ancestor} --not {descendant}`:
/// true when every commit reachable from `ancestor` is also reachable from
/// `descendant`.
pub(crate) fn is_ancestor(
    repo_dir: &Path,
    ancestor: &CommitSha,
    descendant: &CommitSha,
) -> Result<bool> {
    let repo = open(repo_dir)?;
    let ancestor_id = parse_object_id(ancestor.as_str())?;
    let descendant_id = parse_object_id(descendant.as_str())?;

    let mut walk = repo
        .rev_walk([ancestor_id])
        .with_hidden([descendant_id])
        .all()
        .map_err(|err| {
            GitCacheError::Validation(format!("gitoxide rev walk failed to start: {err}"))
        })?;
    match walk.next() {
        Some(Err(err)) => Err(GitCacheError::Validation(format!(
            "gitoxide rev walk failed: {err}"
        ))),
        Some(Ok(_)) => Ok(false),
        None => Ok(true),
    }
}

/// Equivalent of `git cat-file --batch-check='%(objectname) %(objecttype)'`.
/// Missing objects are skipped. Object lookup never lazy-fetches, so this also
/// covers the `GIT_NO_LAZY_FETCH=1` variant.
pub(crate) fn cat_file_batch_types(
    repo_dir: &Path,
    object_ids: &[CommitSha],
) -> Result<HashMap<CommitSha, String>> {
    let repo = open(repo_dir)?;
    let mut types = HashMap::new();
    for object_id in object_ids {
        let oid = parse_object_id(object_id.as_str())?;
        match repo.try_find_header(oid) {
            Ok(Some(header)) => {
                types.insert(object_id.clone(), header.kind().to_string());
            }
            Ok(None) => continue,
            Err(err) => {
                return Err(GitCacheError::Validation(format!(
                    "gitoxide object header lookup failed for {}: {err}",
                    object_id.as_str()
                )));
            }
        }
    }
    Ok(types)
}
