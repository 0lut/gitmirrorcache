//! Advanced performance tests for the API server.

use git_cache_api::app;
use git_cache_core::{AppConfig, GitRemoteConfig, ObjectStoreConfig};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;
use tokio::net::TcpListener;

struct TestServer {
    addr: SocketAddr,
    tmp: TempDir,
    _upstream_work: PathBuf,
    upstream_bare: PathBuf,
}

impl TestServer {
    async fn start() -> Self {
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

        let config = AppConfig {
            bind_addr: addr,
            public_base_url: format!("http://{addr}"),
            cache_root: tmp.path().join("cache"),
            upstream_root: Some(tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 120,
            max_git_output_bytes: 64 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: tmp.path().join("objects"),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            max_concurrent_generation_verifications: 1,
            leases: Default::default(),
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: GitRemoteConfig {
                enabled: true,
                ..Default::default()
            },
            compaction: Default::default(),
        };

        let router = app(config);

        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        let server = Self {
            addr,
            tmp,
            _upstream_work: upstream_work,
            upstream_bare,
        };
        server.warm_all_heads();
        server
    }

    fn git_url(&self, repo: &str) -> String {
        format!("http://{}/git/{}.git", self.addr, repo)
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn run_git_async(cwd: &Path, args: &[&str]) {
    let cwd = cwd.to_path_buf();
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        let output = Command::new("git")
            .current_dir(&cwd)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    })
    .await
    .unwrap();
}

// ── 1. Materialize latency (p50) ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_materialize_latency_p50() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/materialize", server.addr);

    let body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    // Warmup: 5 calls.
    for _ in 0..5 {
        let _ = client.post(&url).json(&body).send().await.unwrap();
    }

    // Measure 50 calls.
    let iterations = 50;
    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let resp = client.post(&url).json(&body).send().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            resp.status().is_success(),
            "materialize failed: {}",
            resp.status()
        );
        latencies.push(elapsed);
    }

    latencies.sort();
    let p50 = latencies[latencies.len() / 2];
    let total: std::time::Duration = latencies.iter().sum();
    eprintln!(
        "materialize latency: {iterations} calls, p50={p50:?}, total={total:?}, avg={:?}",
        total / iterations as u32
    );
    assert!(
        p50.as_millis() < 500,
        "materialize p50 latency too high: {p50:?}"
    );
}

// ── 2. Concurrent materialize throughput ─────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_materialize_throughput() {
    let server = TestServer::start().await;
    let client = Arc::new(reqwest::Client::new());
    let url = format!("http://{}/v1/materialize", server.addr);

    let body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    // Warmup.
    let _ = client.post(&url).json(&body).send().await.unwrap();

    let concurrent_tasks = 20;
    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..concurrent_tasks {
        let client = Arc::clone(&client);
        let url = url.clone();
        let body = body.clone();
        handles.push(tokio::spawn(async move {
            let resp = client.post(&url).json(&body).send().await.unwrap();
            resp.status().as_u16()
        }));
    }

    let mut success = 0u32;
    let mut errors = 0u32;
    for handle in handles {
        let status = handle.await.unwrap();
        if status == 200 {
            success += 1;
        } else {
            errors += 1;
        }
    }
    let elapsed = start.elapsed();

    eprintln!(
        "concurrent materialize: {concurrent_tasks} tasks in {elapsed:?}, success={success}, errors={errors}"
    );
    assert!(
        elapsed.as_secs() < 30,
        "concurrent materialize too slow: {elapsed:?}"
    );
}

// ── 3. Clone + fetch cycle ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_clone_fetch_cycle() {
    let server = TestServer::start().await;
    let url = server.git_url("github.com/org/repo");
    let cycles = 10;

    let start = Instant::now();
    let mut cycle_times = Vec::with_capacity(cycles);
    for i in 0..cycles {
        let cycle_start = Instant::now();

        let clone_dir = server.tmp.path().join(format!("cycle-clone-{i}"));
        run_git_async(
            server.tmp.path(),
            &[
                "clone",
                "--no-tags",
                "--branch",
                "main",
                &url,
                clone_dir.to_str().unwrap(),
            ],
        )
        .await;

        // Push a new commit to the upstream work tree.
        let upstream_work = server.tmp.path().join("work");
        let content = format!("cycle {i}\n");
        let file_path = upstream_work.join("README.md");
        let file_str = file_path.to_str().unwrap().to_string();
        let content_clone = content.clone();
        tokio::task::spawn_blocking(move || {
            std::fs::write(&file_str, content_clone).unwrap();
        })
        .await
        .unwrap();
        run_git_async(&upstream_work, &["add", "README.md"]).await;
        run_git_async(&upstream_work, &["commit", "-m", &format!("cycle {i}")]).await;
        run_git_async(&upstream_work, &["push", "origin", "main"]).await;
        server.warm_all_heads();

        // Fetch in the cloned dir.
        run_git_async(&clone_dir, &["fetch", "origin"]).await;

        let cycle_time = cycle_start.elapsed();
        cycle_times.push(cycle_time);
    }
    let total = start.elapsed();

    let avg = total / cycles as u32;
    eprintln!("clone+fetch cycle: {cycles} cycles in {total:?}, avg={avg:?}");
    assert!(
        total.as_secs() < 120,
        "clone+fetch cycle too slow: {total:?}"
    );
}

// ── 4. Repeated materialize throughput ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_repeated_materialize_throughput() {
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let url = format!("http://{}/v1/materialize", server.addr);

    let body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    let call_count = 20;
    let mut latencies = Vec::with_capacity(call_count);

    let start = Instant::now();
    for _ in 0..call_count {
        let call_start = Instant::now();
        let resp = client.post(&url).json(&body).send().await.unwrap();
        let call_elapsed = call_start.elapsed();
        assert!(
            resp.status().is_success(),
            "materialize failed: {}",
            resp.status()
        );
        latencies.push(call_elapsed);
    }
    let total = start.elapsed();

    let avg = total / call_count as u32;
    eprintln!("repeated materialize: {call_count} calls in {total:?}, avg={avg:?}");
    assert!(
        total.as_secs() < 120,
        "repeated materialize too slow: {total:?}"
    );
}

// ── 5. Metrics endpoint under load ───────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_metrics_endpoint_under_load() {
    let server = TestServer::start().await;
    let client = Arc::new(reqwest::Client::new());
    let url = format!("http://{}/metrics", server.addr);

    let total_requests = 1000;
    let batch_size = 50;
    let batches = total_requests / batch_size;

    let mut error_count = 0u32;
    let start = Instant::now();
    for _ in 0..batches {
        let mut handles = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            let client = Arc::clone(&client);
            let url = url.clone();
            handles.push(tokio::spawn(async move {
                let resp = client.get(&url).send().await.unwrap();
                resp.status().as_u16()
            }));
        }

        for handle in handles {
            let status = handle.await.unwrap();
            if status != 200 {
                error_count += 1;
            }
        }
    }
    let elapsed = start.elapsed();

    eprintln!("metrics under load: {total_requests} requests in {elapsed:?}, errors={error_count}");
    assert_eq!(error_count, 0, "metrics endpoint had {error_count} errors");
    assert!(
        elapsed.as_secs() < 120,
        "metrics under load too slow: {elapsed:?}"
    );
}

// ── 6. Mixed workload ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_mixed_workload() {
    let server = TestServer::start().await;
    let client = Arc::new(reqwest::Client::new());
    let addr = server.addr;
    let tmp_path = server.tmp.path().to_path_buf();

    let materialize_url = format!("http://{addr}/v1/materialize");
    let metrics_url = format!("http://{addr}/metrics");
    let git_url = format!("http://{addr}/git/github.com/org/repo.git");

    let materialize_body = serde_json::json!({
        "repo": "github.com/org/repo",
        "selector": {"branch": "main"}
    });

    let start = Instant::now();
    let mut handles = Vec::new();

    // 5 concurrent materialize calls.
    for _ in 0..5 {
        let client = Arc::clone(&client);
        let url = materialize_url.clone();
        let body = materialize_body.clone();
        handles.push(tokio::spawn(async move {
            let resp = client.post(&url).json(&body).send().await.unwrap();
            ("materialize", resp.status().as_u16())
        }));
    }

    // 5 concurrent clone calls.
    for i in 0..5 {
        let git_url = git_url.clone();
        let tmp_path = tmp_path.clone();
        handles.push(tokio::spawn(async move {
            let clone_dir = tmp_path.join(format!("mixed-clone-{i}"));
            let clone_dir_str = clone_dir.to_str().unwrap().to_string();
            let parent = tmp_path.clone();
            let git_url_clone = git_url.clone();
            let result = tokio::task::spawn_blocking(move || {
                Command::new("git")
                    .current_dir(&parent)
                    .args([
                        "clone",
                        "--no-tags",
                        "--branch",
                        "main",
                        &git_url_clone,
                        &clone_dir_str,
                    ])
                    .output()
            })
            .await
            .unwrap();
            match result {
                Ok(output) if output.status.success() => ("clone", 200u16),
                _ => ("clone", 500u16),
            }
        }));
    }

    // 10 concurrent metrics calls.
    for _ in 0..10 {
        let client = Arc::clone(&client);
        let url = metrics_url.clone();
        handles.push(tokio::spawn(async move {
            let resp = client.get(&url).send().await.unwrap();
            ("metrics", resp.status().as_u16())
        }));
    }

    let mut results = std::collections::HashMap::<&str, (u32, u32)>::new();
    for handle in handles {
        let (kind, status) = handle.await.unwrap();
        let entry = results.entry(kind).or_insert((0, 0));
        if status == 200 {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }
    let elapsed = start.elapsed();

    eprintln!("mixed workload in {elapsed:?}:");
    for (kind, (ok, err)) in &results {
        eprintln!("  {kind}: ok={ok}, err={err}");
    }

    // No crashes: all tasks completed (asserted by await above).
    assert!(
        elapsed.as_secs() < 120,
        "mixed workload too slow: {elapsed:?}"
    );
}
