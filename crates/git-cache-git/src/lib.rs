use bytes::Bytes;
use git_cache_core::{CommitSha, GitCacheError, Result, UpstreamAuth};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;
use tracing::{debug, info};

mod backend;
mod gix_backend;

use backend::{GitBackend, GixBackend, LocalGitBackend};

pub const DEFAULT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Git {
    binary: PathBuf,
    timeout: Duration,
    output_limit: usize,
    extra_env: Vec<(OsString, OsString)>,
    upstream_auth_env: Option<GitAuthEnv>,
    process_semaphore: Arc<Semaphore>,
    local_backend: Arc<dyn LocalGitBackend>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub status_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    /// Decode stdout as UTF-8, mapping failures to a `Validation` error
    /// naming the originating git command.
    pub fn stdout_utf8(self, command: &str) -> Result<String> {
        String::from_utf8(self.stdout).map_err(|err| {
            GitCacheError::Validation(format!("git {command} returned non-utf8: {err}"))
        })
    }

    /// Decode stdout as UTF-8, mapping failures to an `UpstreamUnavailable`
    /// error for commands that talk to a remote.
    pub fn stdout_utf8_upstream(self, command: &str) -> Result<String> {
        String::from_utf8(self.stdout).map_err(|err| {
            GitCacheError::UpstreamUnavailable(format!("{command} returned non-utf8: {err}"))
        })
    }
}

/// Optional flags shared by upstream fetch helpers. All fields are validated
/// (`reject_fetch_filter`, `reject_fetch_depth`) before reaching git argv.
#[derive(Debug, Clone, Copy, Default)]
pub struct FetchOptions<'a> {
    pub filter: Option<&'a str>,
    pub depth: Option<u32>,
    /// Pass `--refetch` so git re-downloads objects it already has locally.
    /// Used to convert a partially hydrated (filtered) repo into one that can
    /// serve full-object clone shapes; plain fetch negotiation would skip
    /// commits whose trees/blobs are absent.
    pub refetch: bool,
    /// Pass `--unshallow` so git removes the repo's shallow boundary. Used
    /// when a full-history intent hits a cache repo previously hydrated with
    /// a depth limit; serving from a shallow repo would emit a pack whose
    /// commit parents are unreadable. Mutually exclusive with `depth`, and
    /// only applied when the repo actually has a `shallow` file (git rejects
    /// `--unshallow` on a complete repository).
    pub unshallow: bool,
}

impl FetchOptions<'_> {
    /// Drop `--unshallow` when the repo at `repo_dir` is not shallow; git
    /// fails with "--unshallow on a complete repository" otherwise. An
    /// earlier fetch in the same request may already have removed the
    /// shallow boundary, so this is re-checked per fetch invocation.
    fn resolve_unshallow(mut self, repo_dir: &Path) -> Self {
        if self.unshallow && !repo_dir.join("shallow").exists() {
            self.unshallow = false;
        }
        self
    }
}

impl Git {
    pub fn new(binary: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self::with_concurrency_limit(
            binary,
            timeout,
            git_cache_core::default_max_concurrent_git_processes(),
        )
    }

    pub fn with_concurrency_limit(
        binary: impl Into<PathBuf>,
        timeout: Duration,
        max_concurrent: usize,
    ) -> Self {
        let effective = max_concurrent.max(1);
        Self {
            binary: binary.into(),
            timeout,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            extra_env: Vec::new(),
            upstream_auth_env: None,
            process_semaphore: Arc::new(Semaphore::new(effective)),
            local_backend: Arc::new(GixBackend),
        }
    }

    pub fn default_with_timeout(timeout: Duration) -> Self {
        Self::new("git", timeout)
    }

    pub fn with_output_limit(mut self, output_limit: usize) -> Self {
        self.output_limit = output_limit;
        self
    }

    /// Select the backend for local read-only operations. In-process
    /// gitoxide is the default; disabling routes everything through the
    /// `git` binary.
    pub fn with_gitoxide(mut self, use_gitoxide: bool) -> Self {
        self.local_backend = if use_gitoxide {
            Arc::new(GixBackend) as Arc<dyn LocalGitBackend>
        } else {
            Arc::new(GitBackend)
        };
        self
    }

    /// Run a synchronous gitoxide operation on the blocking thread pool,
    /// bounded by the same semaphore as git subprocesses.
    async fn run_gix<T, F>(&self, func: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let _permit = self
            .process_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GitCacheError::Internal("git process semaphore closed".into()))?;
        tokio::task::spawn_blocking(func)
            .await
            .map_err(|err| GitCacheError::Internal(format!("gitoxide task failed: {err}")))?
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    pub fn with_upstream_auth(&self, remote_url: &str, auth: &UpstreamAuth) -> Result<Self> {
        reject_remote_url(remote_url)?;
        let mut git = self.clone();
        git.upstream_auth_env = GitAuthEnv::from_upstream_auth(remote_url, auth)?;
        Ok(git)
    }

    pub async fn init_bare(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(None, ["init", "--bare", "--", path_to_str(repo_dir)?])
            .await
    }

    pub async fn rev_parse(&self, repo_dir: &Path, rev: &str) -> Result<String> {
        reject_revision_arg(rev)?;
        self.local_backend
            .clone()
            .rev_parse(self, repo_dir, rev)
            .await
    }

    pub async fn is_ancestor(
        &self,
        repo_dir: &Path,
        ancestor: &CommitSha,
        descendant: &CommitSha,
    ) -> Result<bool> {
        reject_revision_arg(ancestor.as_str())?;
        reject_revision_arg(descendant.as_str())?;
        self.local_backend
            .clone()
            .is_ancestor(self, repo_dir, ancestor, descendant)
            .await
    }

    pub async fn for_each_ref_commits(
        &self,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<CommitSha>> {
        reject_ref_arg(ref_prefix, "ref prefix")?;
        self.local_backend
            .clone()
            .for_each_ref_commits(self, repo_dir, ref_prefix)
            .await
    }

    pub async fn for_each_ref(
        &self,
        repo_dir: &Path,
        ref_prefix: &str,
    ) -> Result<Vec<(String, CommitSha)>> {
        reject_ref_arg(ref_prefix, "ref prefix")?;
        self.local_backend
            .clone()
            .for_each_ref(self, repo_dir, ref_prefix)
            .await
    }

    pub async fn for_each_ref_containing_commit(
        &self,
        repo_dir: &Path,
        commit: &CommitSha,
        ref_prefixes: &[&str],
    ) -> Result<Vec<CommitSha>> {
        reject_revision_arg(commit.as_str())?;
        for ref_prefix in ref_prefixes {
            reject_ref_arg(ref_prefix, "ref prefix")?;
        }

        let contains = format!("--contains={}", commit.as_str());
        let mut args: Vec<OsString> = vec![
            OsString::from("for-each-ref"),
            OsString::from("--format=%(objectname)"),
            OsString::from(contains),
            OsString::from("--"),
        ];
        for ref_prefix in ref_prefixes {
            args.push(OsString::from(ref_prefix));
        }

        let output = self.run(Some(repo_dir), args).await?;
        let text = output.stdout_utf8("for-each-ref")?;
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| CommitSha::parse(line.trim()))
            .collect()
    }

    /// Check, without lazy promisor fetches, that the full snapshot of
    /// `commit` — the commit object plus every tree and blob reachable from
    /// its root tree — is present locally. `--max-count=1` restricts the
    /// walk to that single commit, which is exactly the object set
    /// upload-pack streams for a `--depth 1` clone of the tip.
    pub async fn commit_snapshot_complete_no_lazy(
        &self,
        repo_dir: &Path,
        commit: &CommitSha,
    ) -> Result<bool> {
        reject_revision_arg(commit.as_str())?;
        let git = self.clone().with_env("GIT_NO_LAZY_FETCH", "1");
        let output = match git
            .run(
                Some(repo_dir),
                [
                    "rev-list",
                    "--objects",
                    "--no-object-names",
                    "--missing=print",
                    "--max-count=1",
                    commit.as_str(),
                ],
            )
            .await
        {
            Ok(output) => output,
            // A missing tip (or any other walk failure) means the snapshot
            // is not locally complete; callers fall back to refetching.
            Err(_) => return Ok(false),
        };
        let text = output.stdout_utf8("rev-list")?;
        Ok(!text.lines().any(|line| line.starts_with('?')))
    }

    /// Check, without lazy promisor fetches, that the full object closure
    /// reachable from `commit` is present locally. This is the safety check
    /// for publishing or serving full-history commit wants: a shallow or
    /// partially imported repo can have the tip commit and tree while still
    /// missing parents or older trees/blobs.
    pub async fn commit_history_complete_no_lazy(
        &self,
        repo_dir: &Path,
        commit: &CommitSha,
    ) -> Result<bool> {
        reject_revision_arg(commit.as_str())?;
        // `rev-list --missing=print` respects shallow graft boundaries and
        // would otherwise report a depth-limited repo as complete.
        if repo_dir.join("shallow").exists() {
            return Ok(false);
        }
        let git = self.clone().with_env("GIT_NO_LAZY_FETCH", "1");
        let output = match git
            .run(
                Some(repo_dir),
                [
                    "rev-list",
                    "--objects",
                    "--no-object-names",
                    "--missing=print",
                    commit.as_str(),
                ],
            )
            .await
        {
            Ok(output) => output,
            Err(_) => return Ok(false),
        };
        let text = output.stdout_utf8("rev-list")?;
        Ok(!text.lines().any(|line| line.starts_with('?')))
    }

    pub async fn cat_file_batch_types(
        &self,
        repo_dir: &Path,
        object_ids: &[CommitSha],
    ) -> Result<HashMap<CommitSha, String>> {
        for object_id in object_ids {
            reject_revision_arg(object_id.as_str())?;
        }
        self.local_backend
            .clone()
            .cat_file_batch_types(self, repo_dir, object_ids)
            .await
    }

    pub async fn cat_file_batch_types_no_lazy(
        &self,
        repo_dir: &Path,
        object_ids: &[CommitSha],
    ) -> Result<HashMap<CommitSha, String>> {
        let git = self.clone().with_env("GIT_NO_LAZY_FETCH", "1");
        git.cat_file_batch_types(repo_dir, object_ids).await
    }

    pub async fn fsck(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(Some(repo_dir), ["fsck", "--connectivity-only"])
            .await
    }

    /// Run `git pack-objects --revs` writing a pack containing the objects
    /// reachable from `include` and not reachable from `exclude`. Returns the
    /// path of the written pack file (`{prefix}-{hash}.pack`).
    pub async fn pack_objects_revs(
        &self,
        repo_dir: &Path,
        prefix_path: &Path,
        include: &[CommitSha],
        exclude: &[CommitSha],
    ) -> Result<PathBuf> {
        if include.is_empty() {
            return Err(GitCacheError::Validation(
                "pack-objects requires at least one included revision".into(),
            ));
        }
        let mut stdin = Vec::with_capacity((include.len() + exclude.len()) * 42);
        for commit in include {
            reject_revision_arg(commit.as_str())?;
            stdin.extend_from_slice(commit.as_str().as_bytes());
            stdin.push(b'\n');
        }
        for commit in exclude {
            reject_revision_arg(commit.as_str())?;
            stdin.push(b'^');
            stdin.extend_from_slice(commit.as_str().as_bytes());
            stdin.push(b'\n');
        }

        let args: Vec<OsString> = vec![
            OsString::from("pack-objects"),
            OsString::from("--revs"),
            OsString::from("-q"),
            OsString::from(path_to_str(prefix_path)?),
        ];
        let output = self
            .run_with_stdin_and_limits(
                Some(repo_dir),
                args,
                Some(&stdin),
                self.output_limit,
                self.output_limit,
            )
            .await?;
        let text = output.stdout_utf8("pack-objects")?;
        let hash = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .ok_or_else(|| {
                GitCacheError::Internal("pack-objects did not report a pack name".into())
            })?;
        if hash.is_empty() || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(GitCacheError::Internal(format!(
                "pack-objects reported an invalid pack name `{hash}`"
            )));
        }
        let mut file_name = prefix_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| GitCacheError::Validation("invalid pack prefix path".into()))?
            .to_string();
        file_name.push('-');
        file_name.push_str(hash);
        file_name.push_str(".pack");
        Ok(prefix_path.with_file_name(file_name))
    }

    /// Run `git index-pack` on a pack file already placed in the repo,
    /// generating the companion `.idx` so the objects become readable.
    pub async fn index_pack(&self, repo_dir: &Path, pack_path: &Path) -> Result<GitOutput> {
        self.run(Some(repo_dir), ["index-pack", path_to_str(pack_path)?])
            .await
    }

    /// Apply many ref updates atomically via `git update-ref --stdin`.
    pub async fn update_refs_batch(
        &self,
        repo_dir: &Path,
        updates: &[(String, CommitSha)],
    ) -> Result<GitOutput> {
        if updates.is_empty() {
            return self.run(Some(repo_dir), ["update-ref", "--stdin"]).await;
        }
        let mut stdin = Vec::new();
        for (ref_name, sha) in updates {
            reject_ref_arg(ref_name, "ref")?;
            reject_revision_arg(sha.as_str())?;
            stdin.extend_from_slice(b"update ");
            stdin.extend_from_slice(ref_name.as_bytes());
            stdin.push(b' ');
            stdin.extend_from_slice(sha.as_str().as_bytes());
            stdin.push(b'\n');
        }
        self.run_with_stdin_and_limits(
            Some(repo_dir),
            ["update-ref", "--stdin"],
            Some(&stdin),
            self.output_limit,
            self.output_limit,
        )
        .await
    }

    /// Read a symbolic ref (e.g. `HEAD`) and return its target ref name.
    pub async fn symbolic_ref_read(&self, repo_dir: &Path, name: &str) -> Result<String> {
        reject_ref_arg(name, "symbolic-ref name")?;
        let output = self
            .run(Some(repo_dir), ["symbolic-ref", "--", name])
            .await?;
        Ok(output.stdout_utf8("symbolic-ref")?.trim().to_string())
    }

    pub async fn repack_for_serving(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(
            Some(repo_dir),
            ["-c", "repack.writeBitmaps=false", "repack", "-a", "-d"],
        )
        .await
    }

    /// Write a commit-graph covering all reachable commits so server-side
    /// `pack-objects` and reachability walks on large repos avoid parsing
    /// every commit object.
    pub async fn commit_graph_write(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(Some(repo_dir), ["commit-graph", "write", "--reachable"])
            .await
    }

    /// Run `git ls-remote --symref <remote> HEAD refs/heads/*` and return a map of
    /// `refs/heads/<branch>` → commit SHA, plus the optional default branch name.
    /// The explicit patterns include the HEAD symref without downloading tags.
    pub async fn ls_remote_heads(&self, remote: &str) -> Result<LsRemoteResult> {
        reject_remote_url(remote)?;
        let output = self
            .run_upstream(
                None,
                [
                    "ls-remote",
                    "--symref",
                    "--",
                    remote,
                    "HEAD",
                    "refs/heads/*",
                ],
            )
            .await?;

        let text = output.stdout_utf8_upstream("ls-remote")?;

        let mut refs = HashMap::new();
        let mut default_branch: Option<String> = None;

        for line in text.lines() {
            if let Some(branch) = parse_symref_head_branch(line) {
                default_branch = Some(branch.to_string());
                continue;
            }

            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() == 2 {
                let sha = parts[0].trim();
                let ref_name = parts[1].trim();
                if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
                    refs.insert(branch.to_string(), sha.to_string());
                }
            }
        }

        Ok(LsRemoteResult {
            refs,
            default_branch,
        })
    }

    /// Resolve the default branch via `git ls-remote --symref <remote> HEAD`.
    pub async fn ls_remote_default_branch(&self, remote: &str) -> Result<String> {
        reject_remote_url(remote)?;
        let output = self
            .run_upstream(None, ["ls-remote", "--symref", "--", remote, "HEAD"])
            .await?;

        let text = output.stdout_utf8_upstream("ls-remote")?;

        if let Some(branch) = text.lines().find_map(parse_symref_head_branch) {
            return Ok(branch.to_string());
        }

        Err(GitCacheError::UpstreamUnavailable(
            "upstream did not advertise a symbolic HEAD".into(),
        ))
    }

    pub async fn update_ref(
        &self,
        repo_dir: &Path,
        ref_name: &str,
        sha: &str,
    ) -> Result<GitOutput> {
        reject_ref_arg(ref_name, "ref")?;
        reject_revision_arg(sha)?;
        self.run(Some(repo_dir), ["update-ref", "--", ref_name, sha])
            .await
    }

    pub async fn delete_ref(&self, repo_dir: &Path, ref_name: &str) -> Result<GitOutput> {
        reject_ref_arg(ref_name, "ref")?;
        self.run(Some(repo_dir), ["update-ref", "-d", "--", ref_name])
            .await
    }

    pub async fn symbolic_ref(
        &self,
        repo_dir: &Path,
        name: &str,
        target: &str,
    ) -> Result<GitOutput> {
        reject_ref_arg(name, "symbolic-ref name")?;
        reject_ref_arg(target, "symbolic-ref target")?;
        self.run(Some(repo_dir), ["symbolic-ref", "--", name, target])
            .await
    }

    pub async fn set_config(&self, repo_dir: &Path, key: &str, value: &str) -> Result<GitOutput> {
        reject_config_key(key)?;
        reject_nul(value, "config value")?;
        self.run(Some(repo_dir), ["config", "--local", "--", key, value])
            .await
    }

    /// Spawn `git upload-pack --stateless-rpc .` and return the child process
    /// for streaming. Stdin is written on a background task so large requests
    /// cannot deadlock waiting for the caller to start reading stdout.
    pub async fn upload_pack_spawn(
        &self,
        repo_dir: &Path,
        request_body: Bytes,
    ) -> Result<UploadPackProcess> {
        let permit = self
            .process_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GitCacheError::Internal("git process semaphore closed".into()))?;

        let mut command = Command::new(&self.binary);
        command
            // Bitmap pack reuse is unsafe for the direct-cache serving shape:
            // the advertised refs come from upstream, while the local repo has
            // hidden cache refs and synthetic served refs. Prefer a full
            // traversal over a fast but incomplete pack.
            .args([
                "-c",
                "pack.useBitmaps=false",
                "upload-pack",
                "--stateless-rpc",
                ".",
            ])
            .env_clear()
            // Serving must never trigger promisor lazy fetches: a single
            // pack-objects run over a partial repo can otherwise storm
            // upstream with one fetch per missing object.
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("HOME", "/nonexistent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(repo_dir)
            .kill_on_drop(true);

        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }

        for (key, value) in &self.extra_env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| GitCacheError::Validation("failed to open upload-pack stdin".into()))?;
        tokio::spawn(async move {
            let result = async {
                stdin.write_all(&request_body).await?;
                stdin.shutdown().await
            }
            .await;
            if let Err(err) = result {
                debug!(%err, "failed to write upload-pack request body");
            }
        });

        let stderr = child.stderr.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            GitCacheError::Validation("failed to capture upload-pack stdout".into())
        })?;

        Ok(UploadPackProcess {
            child,
            stdout: Box::pin(stdout),
            stderr,
            timeout: self.timeout,
            stderr_limit: self.output_limit,
            _permit: Some(permit),
        })
    }

    pub async fn fetch_objects(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        object_ids: &[CommitSha],
        mut options: FetchOptions<'_>,
    ) -> Result<GitOutput> {
        reject_remote_url(remote_url)?;
        // Raw exact-oid fetches never need to move the shallow boundary;
        // unshallowing is reserved for ref/all-heads fetches that hydrate
        // commit ancestry.
        options.unshallow = false;
        // Lazy blob fetches can carry tens of thousands of wanted objects;
        // pass them via `--stdin` (like git's own promisor fetch) so the
        // argv stays bounded regardless of want count.
        let mut stdin = Vec::with_capacity(object_ids.len() * 41);
        for object_id in object_ids {
            reject_revision_arg(object_id.as_str())?;
            stdin.extend_from_slice(object_id.as_str().as_bytes());
            stdin.push(b'\n');
        }
        let mut args = fetch_args_with_options(options, remote_url)?;
        // Mirror git's own promisor lazy fetch: raw object ids (blobs/trees)
        // are not revisions, so writing FETCH_HEAD would fail with
        // "bad revision"; negotiation tips are pointless for exact-oid wants.
        args.insert(0, OsString::from("-c"));
        args.insert(1, OsString::from("fetch.negotiationAlgorithm=noop"));
        args.push(OsString::from("--no-write-fetch-head"));
        args.push(OsString::from("--recurse-submodules=no"));
        args.push(OsString::from("--stdin"));
        args.push(OsString::from("--"));
        args.push(OsString::from(remote_url));
        self.run_upstream_with_stdin(Some(repo_dir), args, &stdin)
            .await
    }

    pub async fn fetch_refspecs(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        refspecs: &[String],
        options: FetchOptions<'_>,
    ) -> Result<GitOutput> {
        reject_remote_url(remote_url)?;
        for refspec in refspecs {
            reject_refspec(refspec)?;
        }
        let mut args = fetch_args_with_options(options.resolve_unshallow(repo_dir), remote_url)?;
        args.push(OsString::from("--"));
        args.push(OsString::from(remote_url));
        args.extend(refspecs.iter().map(OsString::from));
        self.run_upstream(Some(repo_dir), args).await
    }

    pub async fn fetch_all_heads(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        options: FetchOptions<'_>,
    ) -> Result<GitOutput> {
        reject_remote_url(remote_url)?;
        let mut args = fetch_args_with_options(options.resolve_unshallow(repo_dir), remote_url)?;
        args.push(OsString::from("--prune"));
        args.extend([
            OsString::from("--"),
            OsString::from(remote_url),
            OsString::from("+refs/heads/*:refs/cache/upstream/heads/*"),
        ]);
        self.run_upstream(Some(repo_dir), args).await
    }

    pub async fn fetch_ref(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        upstream_ref: &str,
        local_ref: &str,
        options: FetchOptions<'_>,
    ) -> Result<GitOutput> {
        reject_remote_url(remote_url)?;
        reject_ref_arg(upstream_ref, "upstream ref")?;
        reject_ref_arg(local_ref, "local ref")?;
        self.check_ref_name(upstream_ref).await?;
        self.check_ref_name(local_ref).await?;

        let refspec = format!("+{upstream_ref}:{local_ref}");
        reject_refspec(&refspec)?;
        let mut args = fetch_args_with_options(options, remote_url)?;
        args.extend([
            OsString::from("--"),
            OsString::from(remote_url),
            OsString::from(refspec),
        ]);
        self.run_upstream(Some(repo_dir), args).await
    }

    async fn check_ref_name(&self, ref_name: &str) -> Result<()> {
        self.run(None, ["check-ref-format", ref_name]).await?;
        Ok(())
    }

    async fn run_with_stdin_and_limits<I, S>(
        &self,
        cwd: Option<&Path>,
        args: I,
        stdin: Option<&[u8]>,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
    ) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin_limits(cwd, args, stdin, max_stdout_bytes, max_stderr_bytes, false)
            .await
    }

    async fn run_upstream<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin_limits(cwd, args, None, self.output_limit, self.output_limit, true)
            .await
            .map_err(|err| self.map_upstream_git_error(err))
    }

    async fn run_upstream_with_stdin<I, S>(
        &self,
        cwd: Option<&Path>,
        args: I,
        stdin: &[u8],
    ) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin_limits(
            cwd,
            args,
            Some(stdin),
            self.output_limit,
            self.output_limit,
            true,
        )
        .await
        .map_err(|err| self.map_upstream_git_error(err))
    }

    async fn run_with_stdin_limits<I, S>(
        &self,
        cwd: Option<&Path>,
        args: I,
        stdin: Option<&[u8]>,
        max_stdout_bytes: usize,
        max_stderr_bytes: usize,
        apply_upstream_auth: bool,
    ) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let _permit = self
            .process_semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| GitCacheError::Internal("git process semaphore closed".into()))?;

        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_os_string())
            .collect();
        debug!(?cwd, ?args, "running git command");

        let mut command = Command::new(&self.binary);
        command
            .args(&args)
            .env_clear()
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("HOME", "/nonexistent")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(tmpdir) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", tmpdir);
        }

        let mut git_config_entries = git_config_entries_from_extra_env(&self.extra_env);
        if apply_upstream_auth {
            if let Some(auth_env) = &self.upstream_auth_env {
                git_config_entries.retain(|(key, _)| key != &auth_env.config_key);
                git_config_entries.push((
                    auth_env.config_key.clone(),
                    OsString::from(auth_env.config_value.clone()),
                ));
            }
        }
        for (key, value) in &self.extra_env {
            if !is_git_config_env_key(key) {
                command.env(key, value);
            }
        }
        apply_git_config_entries(&mut command, &git_config_entries);

        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn()?;
        let mut child_stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| GitCacheError::Validation("failed to capture git stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| GitCacheError::Validation("failed to capture git stderr".to_string()))?;
        let stdin = stdin.map(Vec::from);

        let run = async move {
            let write_stdin = async move {
                if let Some(stdin) = stdin {
                    let mut child_stdin = child_stdin.take().ok_or_else(|| {
                        GitCacheError::Validation("failed to open git stdin".to_string())
                    })?;
                    child_stdin.write_all(&stdin).await?;
                    child_stdin.shutdown().await?;
                }

                Ok(())
            };

            let read_stdout = read_bounded(stdout, max_stdout_bytes, "stdout");
            let read_stderr = read_bounded(stderr, max_stderr_bytes, "stderr");
            let wait_child = async move { child.wait().await.map_err(GitCacheError::from) };

            let ((), stdout, stderr, status) =
                tokio::try_join!(write_stdin, read_stdout, read_stderr, wait_child)?;

            Ok::<_, GitCacheError>((status, stdout, stderr))
        };

        let started = Instant::now();
        let (status, stdout, stderr) = timeout(self.timeout, run).await.map_err(|_| {
            GitCacheError::Timeout(format!("git command exceeded {:?}", self.timeout))
        })??;

        let status_code = status.code().unwrap_or(-1);
        let elapsed = started.elapsed();
        if !status.success() {
            if elapsed >= Duration::from_secs(1) {
                info!(
                    ?cwd,
                    ?args,
                    status_code,
                    elapsed_ms = elapsed.as_millis(),
                    "git command failed"
                );
            }
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(GitCacheError::Validation(format!(
                "git exited with status {status_code}: {stderr}"
            )));
        }
        if elapsed >= Duration::from_secs(1) {
            info!(
                ?cwd,
                ?args,
                status_code,
                elapsed_ms = elapsed.as_millis(),
                "git command finished"
            );
        }

        Ok(GitOutput {
            status_code,
            stdout,
            stderr,
        })
    }

    pub async fn run<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_with_stdin_and_limits(cwd, args, None, self.output_limit, self.output_limit)
            .await
    }

    pub async fn run_no_lazy<I, S>(&self, cwd: Option<&Path>, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let git = self.clone().with_env("GIT_NO_LAZY_FETCH", "1");
        git.run(cwd, args).await
    }

    fn map_upstream_git_error(&self, error: GitCacheError) -> GitCacheError {
        let error_text = error.to_string();
        if self
            .upstream_auth_env
            .as_ref()
            .is_some_and(|auth| auth.authenticated)
            && looks_like_auth_rejection(&error_text)
        {
            return GitCacheError::Unauthorized("upstream rejected authorization".into());
        }

        match error {
            GitCacheError::Validation(message) => GitCacheError::UpstreamUnavailable(format!(
                "upstream git command failed: {message}"
            )),
            other => other,
        }
    }
}

/// Parse a `ref: refs/heads/<branch>\tHEAD` symref line from
/// `ls-remote --symref` output, returning the default branch name.
fn parse_symref_head_branch(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("ref: refs/heads/")?;
    let (branch, target) = rest.split_once('\t')?;
    (target == "HEAD").then_some(branch)
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| GitCacheError::Validation(format!("path is not utf-8: {}", path.display())))
}

fn reject_ref_arg(value: &str, kind: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.contains(':') || value.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid {kind} argument: {value:?}"
        )));
    }

    Ok(())
}

fn reject_revision_arg(value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid revision argument: {value:?}"
        )));
    }

    Ok(())
}

fn reject_config_key(key: &str) -> Result<()> {
    if key.is_empty() || key.starts_with('-') || key.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid config key argument: {key:?}"
        )));
    }
    Ok(())
}

fn reject_remote_url(url: &str) -> Result<()> {
    if url.is_empty() || url.starts_with('-') || url.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid remote URL argument: {url:?}"
        )));
    }
    Ok(())
}

/// Builds a force-fetch refspec that mirrors `refs/heads/{branch}` into
/// `refs/cache/upstream/heads/{branch}`, validating the upstream-supplied
/// branch name before it is interpolated into git arguments.
pub fn branch_cache_refspec(branch: &str) -> Result<String> {
    reject_branch_name(branch)?;
    let upstream_ref = format!("refs/heads/{branch}");
    let local_ref = format!("refs/cache/upstream/heads/{branch}");
    reject_ref_arg(&upstream_ref, "upstream ref")?;
    reject_ref_arg(&local_ref, "local ref")?;
    let refspec = format!("+{upstream_ref}:{local_ref}");
    reject_refspec(&refspec)?;
    Ok(refspec)
}

/// Enforces `git check-ref-format` rules in-process so upstream-supplied
/// branch names cannot smuggle glob patterns or malformed ref syntax into a
/// refspec. Mirrors the documented rules: no control chars, space, `~`, `^`,
/// `:`, `?`, `*`, `[`, or `\`; no `..`, `@{`, or bare `@`; components must be
/// non-empty and must not start with `.`, end with `.`, or end with `.lock`;
/// the name must not start or end with `/` or end with `.`.
fn reject_branch_name(branch: &str) -> Result<()> {
    let invalid = || GitCacheError::Validation(format!("invalid branch name argument: {branch:?}"));
    if branch.is_empty() || branch == "@" {
        return Err(invalid());
    }
    if branch.starts_with('/') || branch.ends_with('/') || branch.ends_with('.') {
        return Err(invalid());
    }
    if branch.contains("..") || branch.contains("@{") {
        return Err(invalid());
    }
    if branch.bytes().any(|b| {
        b < 0x20 || b == 0x7f || matches!(b, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    }) {
        return Err(invalid());
    }
    for component in branch.split('/') {
        if component.is_empty() || component.starts_with('.') || component.ends_with(".lock") {
            return Err(invalid());
        }
    }
    Ok(())
}

fn reject_refspec(refspec: &str) -> Result<()> {
    if refspec.is_empty() || refspec.starts_with('-') || refspec.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid refspec argument: {refspec:?}"
        )));
    }
    Ok(())
}

fn reject_fetch_filter(filter: &str) -> Result<()> {
    if filter != "blob:none" {
        return Err(GitCacheError::Validation(format!(
            "unsupported fetch filter argument: {filter:?}"
        )));
    }
    Ok(())
}

fn reject_fetch_depth(depth: u32) -> Result<()> {
    if depth == 0 {
        return Err(GitCacheError::Validation(
            "fetch depth must be greater than zero".into(),
        ));
    }
    Ok(())
}

fn fetch_args_with_options(options: FetchOptions<'_>, remote_url: &str) -> Result<Vec<OsString>> {
    let mut args = Vec::new();
    if options.filter.is_none() {
        // A filtered (`--filter=blob:none`) fetch persists
        // `remote.<url>.partialclonefilter` in the repo config, and git
        // silently re-applies that saved filter to later unfiltered fetches
        // from the same URL — including `--refetch`, which then still omits
        // blobs. Clear it per-invocation so the explicit `filter` option is
        // the sole source of truth for what each fetch downloads.
        let key = format!("remote.{remote_url}.partialclonefilter");
        reject_config_key(&key)?;
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("{key}=")));
    }
    args.push(OsString::from("fetch"));
    args.push(OsString::from("--no-tags"));
    if let Some(depth) = options.depth {
        reject_fetch_depth(depth)?;
        args.push(OsString::from(format!("--depth={depth}")));
    }
    if let Some(filter) = options.filter {
        reject_fetch_filter(filter)?;
        args.push(OsString::from("--filter=blob:none"));
    }
    if options.refetch {
        args.push(OsString::from("--refetch"));
    }
    if options.unshallow {
        if options.depth.is_some() {
            return Err(GitCacheError::Validation(
                "fetch cannot combine --unshallow with --depth".into(),
            ));
        }
        args.push(OsString::from("--unshallow"));
    }
    Ok(args)
}

fn reject_nul(value: &str, kind: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid {kind} argument: contains NUL byte"
        )));
    }
    Ok(())
}

fn git_config_count_from_extra_env(extra_env: &[(OsString, OsString)]) -> usize {
    extra_env
        .iter()
        .rev()
        .find_map(|(key, value)| {
            if key == OsStr::new("GIT_CONFIG_COUNT") {
                value.to_str()?.parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}

fn git_config_entries_from_extra_env(
    extra_env: &[(OsString, OsString)],
) -> Vec<(String, OsString)> {
    let count = git_config_count_from_extra_env(extra_env);
    let mut keys: HashMap<usize, String> = HashMap::new();
    let mut values: HashMap<usize, OsString> = HashMap::new();

    for (name, value) in extra_env {
        if let Some(slot) = git_config_env_slot(name, "GIT_CONFIG_KEY_") {
            if let Some(key) = value.to_str() {
                keys.insert(slot, key.to_string());
            }
        } else if let Some(slot) = git_config_env_slot(name, "GIT_CONFIG_VALUE_") {
            values.insert(slot, value.clone());
        }
    }

    (0..count)
        .filter_map(|slot| Some((keys.remove(&slot)?, values.remove(&slot)?)))
        .collect()
}

fn git_config_env_slot(name: &OsStr, prefix: &str) -> Option<usize> {
    name.to_str()?.strip_prefix(prefix)?.parse().ok()
}

fn is_git_config_env_key(name: &OsStr) -> bool {
    name == OsStr::new("GIT_CONFIG_COUNT")
        || git_config_env_slot(name, "GIT_CONFIG_KEY_").is_some()
        || git_config_env_slot(name, "GIT_CONFIG_VALUE_").is_some()
}

fn apply_git_config_entries(command: &mut Command, entries: &[(String, OsString)]) {
    if entries.is_empty() {
        return;
    }
    command.env("GIT_CONFIG_COUNT", entries.len().to_string());
    for (slot, (key, value)) in entries.iter().enumerate() {
        command
            .env(format!("GIT_CONFIG_KEY_{slot}"), key)
            .env(format!("GIT_CONFIG_VALUE_{slot}"), value);
    }
}

#[derive(Clone)]
struct GitAuthEnv {
    config_key: String,
    config_value: String,
    authenticated: bool,
}

impl GitAuthEnv {
    fn from_upstream_auth(remote_url: &str, auth: &UpstreamAuth) -> Result<Option<Self>> {
        let Some(raw_header) = auth.raw_header() else {
            return Ok(None);
        };
        let config_key = upstream_extra_header_key(remote_url);
        reject_config_key(&config_key)?;
        reject_nul(raw_header, "upstream authorization header")?;
        Ok(Some(Self {
            config_key,
            config_value: format!("Authorization: {raw_header}"),
            authenticated: auth.is_authenticated(),
        }))
    }
}

impl fmt::Debug for GitAuthEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitAuthEnv")
            .field("config_key", &self.config_key)
            .field("config_value", &"<redacted>")
            .field("authenticated", &self.authenticated)
            .finish()
    }
}

fn upstream_extra_header_key(remote_url: &str) -> String {
    if let Some(rest) = remote_url.strip_prefix("https://") {
        if let Some(host) = rest.split('/').next().filter(|host| !host.is_empty()) {
            return format!("http.https://{host}/.extraHeader");
        }
    }
    "http.https://github.com/.extraHeader".to_string()
}

fn looks_like_auth_rejection(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("authentication failed")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
        || lower.contains("authentication required")
        || lower.contains("authorization failed")
        || lower.contains("permission denied")
}

#[derive(Debug)]
pub struct LsRemoteResult {
    pub refs: HashMap<String, String>,
    pub default_branch: Option<String>,
}

pub struct UploadPackProcess {
    pub child: Child,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    stderr: Option<tokio::process::ChildStderr>,
    timeout: Duration,
    stderr_limit: usize,
    _permit: Option<OwnedSemaphorePermit>,
}

impl UploadPackProcess {
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Take the semaphore permit out of this process, transferring ownership
    /// to the caller. This is useful when the child and stdout are moved into
    /// a separate streaming wrapper that must hold the permit for the full
    /// duration of the response.
    pub fn take_permit(&mut self) -> Option<OwnedSemaphorePermit> {
        self._permit.take()
    }

    /// Wait for the child to finish and check for errors.
    /// Consumes remaining stderr.
    pub async fn wait(mut self) -> Result<()> {
        let stderr_fut = async {
            if let Some(stderr) = self.stderr.take() {
                read_bounded(stderr, self.stderr_limit, "stderr").await
            } else {
                Ok(Vec::new())
            }
        };
        let wait_fut = async { self.child.wait().await.map_err(GitCacheError::from) };
        let (stderr, status) = timeout(self.timeout, async {
            tokio::try_join!(stderr_fut, wait_fut)
        })
        .await
        .map_err(|_| {
            GitCacheError::Timeout(format!("upload-pack exceeded {:?}", self.timeout))
        })??;

        if !status.success() {
            let stderr_text = String::from_utf8_lossy(&stderr);
            return Err(GitCacheError::Validation(format!(
                "upload-pack exited with status {}: {stderr_text}",
                status.code().unwrap_or(-1)
            )));
        }
        Ok(())
    }
}

async fn read_bounded<R>(mut reader: R, max_bytes: usize, stream_name: &str) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(output);
        }

        if output.len().saturating_add(bytes_read) > max_bytes {
            return Err(GitCacheError::Validation(format!(
                "git {stream_name} exceeded limit of {max_bytes} bytes"
            )));
        }

        output.extend_from_slice(&buffer[..bytes_read]);
    }
}

#[cfg(test)]
mod tests;
