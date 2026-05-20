use git_cache_core::{GitCacheError, Result};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use tracing::debug;

pub const DEFAULT_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Git {
    binary: PathBuf,
    timeout: Duration,
    output_limit: usize,
    extra_env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitOutput {
    pub status_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Git {
    pub fn new(binary: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            binary: binary.into(),
            timeout,
            output_limit: DEFAULT_OUTPUT_LIMIT,
            extra_env: Vec::new(),
        }
    }

    pub fn default_with_timeout(timeout: Duration) -> Self {
        Self::new("git", timeout)
    }

    pub fn with_output_limit(mut self, output_limit: usize) -> Self {
        self.output_limit = output_limit;
        self
    }

    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    pub async fn init_bare(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(None, ["init", "--bare", "--", path_to_str(repo_dir)?])
            .await
    }

    pub async fn fetch_branch(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        branch: &str,
        local_ref: &str,
    ) -> Result<GitOutput> {
        reject_ref_arg(branch, "branch")?;
        reject_ref_arg(local_ref, "local ref")?;
        self.check_branch_name(branch).await?;
        self.check_ref_name(local_ref).await?;

        let refspec = format!("refs/heads/{branch}:{local_ref}");
        self.run(
            Some(repo_dir),
            ["fetch", "--no-tags", "--", remote_url, &refspec],
        )
        .await
    }

    pub async fn rev_parse(&self, repo_dir: &Path, rev: &str) -> Result<String> {
        reject_revision_arg(rev)?;
        let output = self
            .run(
                Some(repo_dir),
                ["rev-parse", "--verify", "--end-of-options", rev],
            )
            .await?;
        String::from_utf8(output.stdout)
            .map(|value| value.trim().to_string())
            .map_err(|err| {
                GitCacheError::Validation(format!("git rev-parse returned non-utf8: {err}"))
            })
    }

    pub async fn fsck(&self, repo_dir: &Path) -> Result<GitOutput> {
        self.run(Some(repo_dir), ["fsck", "--connectivity-only"])
            .await
    }

    pub async fn bundle_create(
        &self,
        repo_dir: &Path,
        bundle_path: &Path,
        rev: &str,
    ) -> Result<GitOutput> {
        reject_revision_arg(rev)?;
        self.run(
            Some(repo_dir),
            ["bundle", "create", path_to_str(bundle_path)?, rev],
        )
        .await
    }

    pub async fn bundle_create_all(
        &self,
        repo_dir: &Path,
        bundle_path: &Path,
    ) -> Result<GitOutput> {
        self.run(
            Some(repo_dir),
            ["bundle", "create", path_to_str(bundle_path)?, "--all"],
        )
        .await
    }

    pub async fn fetch_bundle(&self, repo_dir: &Path, bundle_path: &Path) -> Result<GitOutput> {
        self.run(
            Some(repo_dir),
            ["fetch", "--", path_to_str(bundle_path)?, "+refs/*:refs/*"],
        )
        .await
    }

    pub async fn upload_pack_advertise_refs(
        &self,
        repo_dir: &Path,
        max_output_bytes: usize,
    ) -> Result<GitOutput> {
        self.run_with_stdin_and_limits(
            Some(repo_dir),
            ["upload-pack", "--stateless-rpc", "--advertise-refs", "."],
            None,
            max_output_bytes,
            self.output_limit,
        )
        .await
    }

    pub async fn upload_pack_stateless_rpc(
        &self,
        repo_dir: &Path,
        request: &[u8],
        max_request_bytes: usize,
        max_output_bytes: usize,
    ) -> Result<GitOutput> {
        if request.len() > max_request_bytes {
            return Err(GitCacheError::Validation(format!(
                "git upload-pack request exceeded limit of {max_request_bytes} bytes"
            )));
        }

        self.run_with_stdin_and_limits(
            Some(repo_dir),
            ["upload-pack", "--stateless-rpc", "."],
            Some(request),
            max_output_bytes,
            self.output_limit,
        )
        .await
    }

    /// Run `git ls-remote --symref <remote>` and return a map of
    /// `refs/heads/<branch>` → commit SHA, plus the optional default branch name.
    /// We intentionally omit `--heads` so that the HEAD symref annotation is
    /// included in the output, and filter to `refs/heads/*` in memory.
    pub async fn ls_remote_heads(&self, remote: &str) -> Result<LsRemoteResult> {
        reject_remote_url(remote)?;
        let output = self
            .run(None, ["ls-remote", "--symref", "--", remote])
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;

        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::UpstreamUnavailable(format!("ls-remote returned non-utf8: {err}"))
        })?;

        let mut refs = HashMap::new();
        let mut default_branch: Option<String> = None;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ref: refs/heads/") {
                if let Some((branch, target)) = rest.split_once('\t') {
                    if target == "HEAD" {
                        default_branch = Some(branch.to_string());
                    }
                }
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
            .run(None, ["ls-remote", "--symref", "--", remote, "HEAD"])
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))?;

        let text = String::from_utf8(output.stdout).map_err(|err| {
            GitCacheError::UpstreamUnavailable(format!("ls-remote returned non-utf8: {err}"))
        })?;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("ref: refs/heads/") {
                if let Some((branch, head)) = rest.split_once('\t') {
                    if head == "HEAD" {
                        return Ok(branch.to_string());
                    }
                }
            }
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
    /// for streaming. The caller is responsible for writing stdin, reading
    /// stdout, and waiting on the child.
    pub async fn upload_pack_spawn(
        &self,
        repo_dir: &Path,
        request_body: &[u8],
    ) -> Result<UploadPackProcess> {
        let mut command = Command::new(&self.binary);
        command
            .args(["upload-pack", "--stateless-rpc", "."])
            .env_clear()
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

        for (key, value) in &self.extra_env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(request_body).await?;
            stdin.shutdown().await?;
        }

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
        })
    }

    /// Spawn `git upload-pack --stateless-rpc --advertise-refs .` and return
    /// stdout for streaming.
    pub async fn upload_pack_advertise_refs_spawn(
        &self,
        repo_dir: &Path,
    ) -> Result<UploadPackProcess> {
        let mut command = Command::new(&self.binary);
        command
            .args(["upload-pack", "--stateless-rpc", "--advertise-refs", "."])
            .env_clear()
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ASKPASS", "/bin/false")
            .env("SSH_ASKPASS", "/bin/false")
            .env("HOME", "/nonexistent")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(repo_dir)
            .kill_on_drop(true);

        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }

        for (key, value) in &self.extra_env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
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
        })
    }

    pub async fn fetch_refs(
        &self,
        repo_dir: &Path,
        remote_url: &str,
        refspecs: &[String],
    ) -> Result<GitOutput> {
        reject_remote_url(remote_url)?;
        for refspec in refspecs {
            reject_refspec(refspec)?;
        }
        let mut args: Vec<String> = vec![
            "fetch".to_string(),
            "--no-tags".to_string(),
            "--".to_string(),
            remote_url.to_string(),
        ];
        args.extend(refspecs.iter().cloned());
        let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.run(Some(repo_dir), args_ref)
            .await
            .map_err(|err| GitCacheError::UpstreamUnavailable(err.to_string()))
    }

    async fn check_branch_name(&self, branch: &str) -> Result<()> {
        self.run(None, ["check-ref-format", "--branch", branch])
            .await?;
        Ok(())
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

        for (key, value) in &self.extra_env {
            command.env(key, value);
        }

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

        let (status, stdout, stderr) = timeout(self.timeout, run).await.map_err(|_| {
            GitCacheError::Timeout(format!("git command exceeded {:?}", self.timeout))
        })??;

        let status_code = status.code().unwrap_or(-1);
        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            return Err(GitCacheError::Validation(format!(
                "git exited with status {status_code}: {stderr}"
            )));
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

fn reject_refspec(refspec: &str) -> Result<()> {
    if refspec.is_empty() || refspec.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid refspec argument: {refspec:?}"
        )));
    }
    Ok(())
}

fn reject_nul(value: &str, kind: &str) -> Result<()> {
    if value.contains('\0') {
        return Err(GitCacheError::Validation(format!(
            "invalid {kind} argument: contains NUL byte"
        )));
    }
    Ok(())
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
}

impl UploadPackProcess {
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
mod tests {
    use super::*;

    // ── reject_ref_arg tests ────────────────────────────────────────

    #[test]
    fn reject_ref_arg_rejects_empty() {
        assert!(reject_ref_arg("", "ref").is_err());
    }

    #[test]
    fn reject_ref_arg_rejects_leading_dash() {
        assert!(reject_ref_arg("-evil", "ref").is_err());
        assert!(reject_ref_arg("--flag", "ref").is_err());
    }

    #[test]
    fn reject_ref_arg_rejects_colon() {
        assert!(reject_ref_arg("HEAD:path", "ref").is_err());
    }

    #[test]
    fn reject_ref_arg_rejects_nul() {
        assert!(reject_ref_arg("ref\0name", "ref").is_err());
    }

    #[test]
    fn reject_ref_arg_accepts_valid() {
        assert!(reject_ref_arg("refs/heads/main", "ref").is_ok());
        assert!(reject_ref_arg("feature/test", "ref").is_ok());
    }

    // ── reject_revision_arg tests ───────────────────────────────────

    #[test]
    fn reject_revision_arg_rejects_empty() {
        assert!(reject_revision_arg("").is_err());
    }

    #[test]
    fn reject_revision_arg_rejects_leading_dash() {
        assert!(reject_revision_arg("-evil").is_err());
    }

    #[test]
    fn reject_revision_arg_rejects_nul() {
        assert!(reject_revision_arg("rev\0ision").is_err());
    }

    #[test]
    fn reject_revision_arg_allows_colon() {
        assert!(reject_revision_arg("HEAD:path").is_ok());
    }

    #[test]
    fn reject_revision_arg_accepts_valid() {
        assert!(reject_revision_arg("abc123").is_ok());
        assert!(reject_revision_arg("HEAD^{commit}").is_ok());
    }

    // ── reject_config_key tests ─────────────────────────────────────

    #[test]
    fn reject_config_key_rejects_empty() {
        assert!(reject_config_key("").is_err());
    }

    #[test]
    fn reject_config_key_rejects_leading_dash() {
        assert!(reject_config_key("-bad").is_err());
    }

    #[test]
    fn reject_config_key_rejects_nul() {
        assert!(reject_config_key("key\0val").is_err());
    }

    #[test]
    fn reject_config_key_allows_equals() {
        assert!(reject_config_key("key=value").is_ok());
    }

    // ── reject_remote_url tests ─────────────────────────────────────

    #[test]
    fn reject_remote_url_rejects_empty() {
        assert!(reject_remote_url("").is_err());
    }

    #[test]
    fn reject_remote_url_rejects_leading_dash() {
        assert!(reject_remote_url("-evil").is_err());
    }

    #[test]
    fn reject_remote_url_rejects_nul() {
        assert!(reject_remote_url("url\0bad").is_err());
    }

    #[test]
    fn reject_remote_url_accepts_valid() {
        assert!(reject_remote_url("https://github.com/org/repo.git").is_ok());
        assert!(reject_remote_url("/path/to/repo").is_ok());
    }

    // ── reject_refspec tests ────────────────────────────────────────

    #[test]
    fn reject_refspec_rejects_empty() {
        assert!(reject_refspec("").is_err());
    }

    #[test]
    fn reject_refspec_rejects_nul() {
        assert!(reject_refspec("spec\0bad").is_err());
    }

    #[test]
    fn reject_refspec_allows_leading_plus() {
        assert!(reject_refspec("+refs/heads/main:refs/heads/main").is_ok());
    }

    #[test]
    fn reject_refspec_allows_colon() {
        assert!(reject_refspec("refs/heads/main:refs/remotes/origin/main").is_ok());
    }

    // ── reject_nul tests ────────────────────────────────────────────

    #[test]
    fn reject_nul_rejects_nul_byte() {
        assert!(reject_nul("hello\0world", "value").is_err());
    }

    #[test]
    fn reject_nul_accepts_clean_string() {
        assert!(reject_nul("hello world", "value").is_ok());
    }

    // ── Public method rejection of dash-prefixed arguments ──────────

    fn test_git() -> Git {
        Git::default_with_timeout(Duration::from_secs(1))
    }

    #[tokio::test]
    async fn fetch_branch_rejects_dash_branch() {
        let git = test_git();
        assert!(git
            .fetch_branch(Path::new("/unused"), "url", "-evil", "refs/cache/test")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_branch_rejects_dash_local_ref() {
        let git = test_git();
        assert!(git
            .fetch_branch(Path::new("/unused"), "url", "main", "--evil")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rev_parse_rejects_dash_rev() {
        let git = test_git();
        assert!(git.rev_parse(Path::new("/unused"), "--evil").await.is_err());
    }

    #[tokio::test]
    async fn bundle_create_rejects_dash_rev() {
        let git = test_git();
        assert!(git
            .bundle_create(Path::new("/unused"), Path::new("/unused"), "-evil")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn update_ref_rejects_dash_ref_name() {
        let git = test_git();
        assert!(git
            .update_ref(Path::new("/unused"), "-evil", "abc123")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn symbolic_ref_rejects_dash_name() {
        let git = test_git();
        assert!(git
            .symbolic_ref(Path::new("/unused"), "--evil", "refs/heads/main")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn set_config_rejects_dash_key() {
        let git = test_git();
        assert!(git
            .set_config(Path::new("/unused"), "--evil", "value")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn ls_remote_heads_rejects_dash_url() {
        let git = test_git();
        assert!(git.ls_remote_heads("-evil").await.is_err());
    }

    #[tokio::test]
    async fn ls_remote_default_branch_rejects_dash_url() {
        let git = test_git();
        assert!(git.ls_remote_default_branch("-evil").await.is_err());
    }

    #[tokio::test]
    async fn fetch_refs_rejects_dash_url() {
        let git = test_git();
        assert!(git
            .fetch_refs(Path::new("/unused"), "-evil", &[])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn fetch_refs_rejects_nul_in_refspec() {
        let git = test_git();
        assert!(git
            .fetch_refs(
                Path::new("/unused"),
                "https://example.com/repo.git",
                &["bad\0spec".to_string()]
            )
            .await
            .is_err());
    }
}
