//! Local read-only operation backends.
//!
//! `Git` delegates local read-only operations (`rev_parse`, ref listing,
//! ancestry checks, object type lookups) to a [`LocalGitBackend`]. Two
//! implementations exist: [`GixBackend`] runs in-process via gitoxide, and
//! [`GitBackend`] shells out to the `git` binary. Input validation happens in
//! the `Git` wrapper before dispatch, so backends receive pre-validated
//! arguments.

use async_trait::async_trait;
use git_cache_core::{CommitSha, GitCacheError, Result};
use std::collections::HashMap;
use std::path::Path;

use crate::{gix_backend, Git};

#[async_trait]
pub(crate) trait LocalGitBackend: Send + Sync + std::fmt::Debug {
    async fn rev_parse(&self, git: &Git, repo_dir: &Path, rev: &str) -> Result<String>;

    async fn is_ancestor(
        &self,
        git: &Git,
        repo_dir: &Path,
        ancestor: &CommitSha,
        descendant: &CommitSha,
    ) -> Result<bool>;

    async fn for_each_ref_commits(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<CommitSha>>;

    async fn for_each_ref(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<(String, CommitSha)>>;

    async fn cat_file_batch_types(
        &self,
        git: &Git,
        repo_dir: &Path,
        object_ids: &[CommitSha],
    ) -> Result<HashMap<CommitSha, String>>;
}

/// In-process gitoxide implementation. Operations run on the blocking thread
/// pool, bounded by the same semaphore as git subprocesses.
#[derive(Debug)]
pub(crate) struct GixBackend;

#[async_trait]
impl LocalGitBackend for GixBackend {
    async fn rev_parse(&self, git: &Git, repo_dir: &Path, rev: &str) -> Result<String> {
        let repo_dir = repo_dir.to_path_buf();
        let rev = rev.to_string();
        git.run_gix(move || gix_backend::rev_parse(&repo_dir, &rev))
            .await
    }

    async fn is_ancestor(
        &self,
        git: &Git,
        repo_dir: &Path,
        ancestor: &CommitSha,
        descendant: &CommitSha,
    ) -> Result<bool> {
        let repo_dir = repo_dir.to_path_buf();
        let ancestor = ancestor.clone();
        let descendant = descendant.clone();
        git.run_gix(move || gix_backend::is_ancestor(&repo_dir, &ancestor, &descendant))
            .await
    }

    async fn for_each_ref_commits(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<CommitSha>> {
        let repo_dir = repo_dir.to_path_buf();
        let ref_prefix = ref_prefix.to_string();
        git.run_gix(move || gix_backend::for_each_ref_commits(&repo_dir, &ref_prefix))
            .await
    }

    async fn for_each_ref(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<(String, CommitSha)>> {
        let repo_dir = repo_dir.to_path_buf();
        let ref_prefix = ref_prefix.to_string();
        git.run_gix(move || gix_backend::for_each_ref(&repo_dir, &ref_prefix))
            .await
    }

    async fn cat_file_batch_types(
        &self,
        git: &Git,
        repo_dir: &Path,
        object_ids: &[CommitSha],
    ) -> Result<HashMap<CommitSha, String>> {
        let repo_dir = repo_dir.to_path_buf();
        let object_ids = object_ids.to_vec();
        git.run_gix(move || gix_backend::cat_file_batch_types(&repo_dir, &object_ids))
            .await
    }
}

/// Subprocess implementation backed by the `git` binary.
#[derive(Debug)]
pub(crate) struct GitBackend;

#[async_trait]
impl LocalGitBackend for GitBackend {
    async fn rev_parse(&self, git: &Git, repo_dir: &Path, rev: &str) -> Result<String> {
        let output = git
            .run(
                Some(repo_dir),
                ["rev-parse", "--verify", "--end-of-options", rev],
            )
            .await?;
        output
            .stdout_utf8("rev-parse")
            .map(|value| value.trim().to_string())
    }

    async fn is_ancestor(
        &self,
        git: &Git,
        repo_dir: &Path,
        ancestor: &CommitSha,
        descendant: &CommitSha,
    ) -> Result<bool> {
        let output = git
            .run(
                Some(repo_dir),
                [
                    "rev-list",
                    "--max-count=1",
                    ancestor.as_str(),
                    "--not",
                    descendant.as_str(),
                    "--",
                ],
            )
            .await?;
        Ok(output.stdout.iter().all(|byte| byte.is_ascii_whitespace()))
    }

    async fn for_each_ref_commits(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<CommitSha>> {
        let output = git
            .run(
                Some(repo_dir),
                ["for-each-ref", "--format=%(objectname)", "--", ref_prefix],
            )
            .await?;
        let text = output.stdout_utf8("for-each-ref")?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| CommitSha::parse(line.trim()))
            .collect()
    }

    async fn for_each_ref(
        &self,
        git: &Git,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<(String, CommitSha)>> {
        let output = git
            .run(
                Some(repo_dir),
                [
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "--",
                    ref_prefix,
                ],
            )
            .await?;
        let text = output.stdout_utf8("for-each-ref")?;
        let mut refs = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((ref_name, commit)) = line.split_once(' ') else {
                return Err(GitCacheError::Validation(format!(
                    "malformed git for-each-ref output line: {line:?}"
                )));
            };
            refs.push((ref_name.to_string(), CommitSha::parse(commit)?));
        }
        Ok(refs)
    }

    async fn cat_file_batch_types(
        &self,
        git: &Git,
        repo_dir: &Path,
        object_ids: &[CommitSha],
    ) -> Result<HashMap<CommitSha, String>> {
        let mut stdin = Vec::with_capacity(object_ids.len() * 41);
        for object_id in object_ids {
            stdin.extend_from_slice(object_id.as_str().as_bytes());
            stdin.push(b'\n');
        }

        let output = git
            .run_with_stdin_and_limits(
                Some(repo_dir),
                ["cat-file", "--batch-check=%(objectname) %(objecttype)"],
                Some(&stdin),
                git.output_limit,
                git.output_limit,
            )
            .await?;
        let text = output.stdout_utf8("cat-file")?;

        let mut types = HashMap::new();
        for line in text.lines() {
            let Some((object_id, object_type)) = line.split_once(' ') else {
                return Err(GitCacheError::Validation(format!(
                    "malformed git cat-file output line: {line:?}"
                )));
            };
            if object_type == "missing" {
                continue;
            }
            types.insert(CommitSha::parse(object_id)?, object_type.to_string());
        }
        Ok(types)
    }
}
