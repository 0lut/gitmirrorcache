use git_cache_core::{GitCacheError, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
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
