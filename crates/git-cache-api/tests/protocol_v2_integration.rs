//! Integration tests for Git wire protocol v2 on the direct Git endpoint,
//! including bundle-uri advertisement from published generation bundles.
//!
//! These spin up a real Axum server with a local upstream and run actual
//! `git` clients pinned to protocol v2 against it.

mod support;

mod tests {
    use super::support;

    use git_cache_api::app;
    use std::net::SocketAddr;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    struct TestServer {
        addr: SocketAddr,
        tmp: TempDir,
        upstream_work: PathBuf,
        upstream_bare: PathBuf,
    }

    impl TestServer {
        async fn start(bundle_uri: bool) -> Self {
            Self::start_with_warm(bundle_uri, true).await
        }

        // Materializing an already-warm cache records manifests without
        // publishing a generation bundle, so bundle-uri tests start cold.
        async fn start_with_warm(bundle_uri: bool, warm: bool) -> Self {
            let tmp = TempDir::new().unwrap();
            let upstream_bare = tmp.path().join("upstreams/github.com/org/repo.git");
            let upstream_work = tmp.path().join("work");

            std::fs::create_dir_all(upstream_bare.parent().unwrap()).unwrap();
            std::fs::create_dir_all(&upstream_work).unwrap();

            run_git(
                tmp.path(),
                &["init", "--bare", upstream_bare.to_str().unwrap()],
            );
            run_git(&upstream_work, &["init"]);
            run_git(
                &upstream_work,
                &["config", "user.email", "test@example.com"],
            );
            run_git(&upstream_work, &["config", "user.name", "Test"]);
            std::fs::write(upstream_work.join("README.md"), "initial\n").unwrap();
            run_git(&upstream_work, &["add", "README.md"]);
            run_git(&upstream_work, &["commit", "-m", "initial"]);
            run_git(&upstream_work, &["branch", "-M", "main"]);
            run_git(
                &upstream_work,
                &["remote", "add", "origin", upstream_bare.to_str().unwrap()],
            );
            run_git(&upstream_work, &["push", "origin", "main"]);
            run_git(&upstream_bare, &["symbolic-ref", "HEAD", "refs/heads/main"]);

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            let mut config = support::test_config(addr, tmp.path());
            if bundle_uri {
                config.git_remote.bundle_uri_enabled = true;
                config.git_remote.bundle_uri_base_url =
                    Some(format!("file://{}", tmp.path().join("objects").display()));
            }

            let router = app(config);
            tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });

            let server = Self {
                addr,
                tmp,
                upstream_work,
                upstream_bare,
            };
            if warm {
                server.warm_all_heads();
            }
            server
        }

        fn git_url(&self, repo: &str) -> String {
            format!("http://{}/git/{}.git", self.addr, repo)
        }

        fn head_commit(&self) -> String {
            git_stdout(&self.upstream_work, &["rev-parse", "HEAD"])
        }

        async fn materialize_main(&self) {
            let response = reqwest::Client::new()
                .post(format!("http://{}/v1/materialize", self.addr))
                .json(&serde_json::json!({
                    "repo": "github.com/org/repo",
                    "selector": {"branch": "main"}
                }))
                .send()
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "materialize failed: {}",
                response.status()
            );
        }

        fn warm_all_heads(&self) {
            let repo_dir = self.tmp.path().join("cache/repos/github.com/org/repo.git");
            if !repo_dir.join("config").exists() {
                std::fs::create_dir_all(repo_dir.parent().unwrap()).unwrap();
                run_git(
                    self.tmp.path(),
                    &["init", "--bare", repo_dir.to_str().unwrap()],
                );
            }
            run_git(
                &repo_dir,
                &[
                    "fetch",
                    "--no-tags",
                    self.upstream_bare.to_str().unwrap(),
                    "+refs/heads/*:refs/cache/upstream/heads/*",
                    "+refs/heads/*:refs/heads/*",
                ],
            );
            run_git(&repo_dir, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    async fn run_git_async(cwd: &Path, args: &[&str]) -> (bool, String) {
        run_git_async_with_env(cwd, args, &[]).await
    }

    fn git_supports_bundle_uri() -> bool {
        let output = Command::new("git").arg("version").output().unwrap();
        let version = String::from_utf8_lossy(&output.stdout);
        let Some(rest) = version.trim().strip_prefix("git version ") else {
            return false;
        };
        let mut parts = rest.split('.');
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        major > 2 || (major == 2 && minor >= 38)
    }

    async fn run_git_async_with_env(
        cwd: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> (bool, String) {
        let cwd = cwd.to_path_buf();
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let envs: Vec<(String, String)> = envs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        tokio::task::spawn_blocking(move || {
            let output = Command::new("git")
                .current_dir(&cwd)
                .args(&args)
                .envs(envs)
                .output()
                .unwrap();
            (
                output.status.success(),
                format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            )
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_v2_ls_remote_lists_refs() {
        let server = TestServer::start(false).await;
        let (ok, output) = run_git_async(
            server.tmp.path(),
            &[
                "-c",
                "protocol.version=2",
                "ls-remote",
                "--symref",
                &server.git_url("github.com/org/repo"),
            ],
        )
        .await;
        assert!(ok, "ls-remote failed: {output}");
        assert!(output.contains("refs/heads/main"), "{output}");
        assert!(
            output.contains("ref: refs/heads/main\tHEAD"),
            "missing HEAD symref: {output}"
        );
        assert!(output.contains(&server.head_commit()), "{output}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_v2_clone_succeeds() {
        let server = TestServer::start(false).await;
        let clone_dir = server.tmp.path().join("clone-v2");
        let (ok, output) = run_git_async(
            server.tmp.path(),
            &[
                "-c",
                "protocol.version=2",
                "clone",
                &server.git_url("github.com/org/repo"),
                clone_dir.to_str().unwrap(),
            ],
        )
        .await;
        assert!(ok, "clone failed: {output}");
        let head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head, server.head_commit());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn protocol_v2_clone_with_bundle_uri_uses_generation_bundles() {
        let server = TestServer::start_with_warm(true, false).await;
        server.materialize_main().await;

        let clone_dir = server.tmp.path().join("clone-bundle");
        let trace_path = server.tmp.path().join("trace2.log");
        let (ok, output) = run_git_async_with_env(
            server.tmp.path(),
            &[
                "-c",
                "protocol.version=2",
                "-c",
                "transfer.bundleURI=true",
                "clone",
                &server.git_url("github.com/org/repo"),
                clone_dir.to_str().unwrap(),
            ],
            &[("GIT_TRACE2_EVENT", trace_path.to_str().unwrap())],
        )
        .await;
        assert!(ok, "bundle-uri clone failed: {output}");
        let head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head, server.head_commit());

        // `transfer.bundleURI` client support landed in git 2.38; older
        // clients ignore the advertisement and fall back to a normal clone.
        if git_supports_bundle_uri() {
            let trace = std::fs::read_to_string(&trace_path).unwrap_or_default();
            assert!(
                trace.contains("bundle-uri"),
                "client never used bundle-uri: {trace}"
            );
        }
    }

    fn pkt_line(data: &str) -> Vec<u8> {
        let mut out = format!("{:04x}", 4 + data.len()).into_bytes();
        out.extend_from_slice(data.as_bytes());
        out
    }

    async fn post_v2_command(server: &TestServer, command: &str) -> (u16, String) {
        let mut body = pkt_line(&format!("command={command}\n"));
        body.extend_from_slice(b"0001");
        body.extend_from_slice(b"0000");
        let response = reqwest::Client::new()
            .post(format!(
                "{}/git-upload-pack",
                server.git_url("github.com/org/repo")
            ))
            .header("Content-Type", "application/x-git-upload-pack-request")
            .header("Git-Protocol", "version=2")
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        (status, body)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_uri_command_advertises_generation_bundles() {
        let server = TestServer::start_with_warm(true, false).await;
        server.materialize_main().await;

        let (status, body) = post_v2_command(&server, "bundle-uri").await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("bundle.version=1"), "{body}");
        assert!(body.contains("bundle.mode=all"), "{body}");
        assert!(body.contains(".uri=file://"), "{body}");
        assert!(body.contains(".bundle"), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_uri_command_returns_empty_list_when_disabled() {
        let server = TestServer::start(false).await;
        let (status, body) = post_v2_command(&server, "bundle-uri").await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("bundle.version=1"), "{body}");
        assert!(!body.contains(".uri="), "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_v2_command_is_rejected() {
        let server = TestServer::start(false).await;
        let (status, body) = post_v2_command(&server, "object-info").await;
        assert_eq!(status, 400, "{body}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_uri_disabled_keeps_clone_working() {
        let server = TestServer::start(false).await;
        let clone_dir = server.tmp.path().join("clone-no-bundle");
        let (ok, output) = run_git_async(
            server.tmp.path(),
            &[
                "-c",
                "protocol.version=2",
                "-c",
                "transfer.bundleURI=true",
                "clone",
                &server.git_url("github.com/org/repo"),
                clone_dir.to_str().unwrap(),
            ],
        )
        .await;
        assert!(ok, "clone failed: {output}");
        let head = git_stdout(&clone_dir, &["rev-parse", "HEAD"]);
        assert_eq!(head, server.head_commit());
    }
}
