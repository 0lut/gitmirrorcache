//! Randomized end-to-end fuzzing of the domain `Materializer`.
//!
//! Real local git upstreams are mutated while many tasks concurrently
//! materialize/resolve random selectors and race repo invalidation. This
//! exercises the full stack: git subprocess semaphore, disk reservations,
//! repo locks, generation manifests, and the serving-maintenance dedup set.
//! Operations may fail (e.g. unknown commit, invalidation conflict); they
//! must never panic, deadlock, or corrupt the cache beyond recovery.

use git_cache_core::{
    AppConfig, BranchName, CommitSha, MaterializeRequest, ObjectStoreConfig, RepoKey, Selector,
};
use git_cache_domain::{AppState, Materializer};
use git_cache_fuzz::FuzzConfig;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

struct Fixture {
    tmp: TempDir,
    repo: RepoKey,
}

impl Fixture {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let repo = RepoKey::parse("github.com/fuzz/repo").expect("repo key");
        let fixture = Self { tmp, repo };
        fixture.init();
        fixture
    }

    fn config(&self) -> AppConfig {
        AppConfig {
            bind_addr: "127.0.0.1:0".parse().expect("addr"),
            cache_root: self.tmp.path().join("cache"),
            upstream_root: Some(self.tmp.path().join("upstreams")),
            git_binary: PathBuf::from("git"),
            git_timeout_seconds: 60,
            max_git_output_bytes: 16 * 1024 * 1024,
            object_store: ObjectStoreConfig::Local {
                root: self.tmp.path().join("objects"),
            },
            upstream_auth_token_env: None,
            rate_limit_per_minute: 0,
            allowed_upstream_hosts: vec!["github.com".into()],
            disk: git_cache_core::DiskConfig {
                // Small quota keeps LRU eviction racing materialization.
                quota_bytes: 256 * 1024 * 1024,
                min_free_bytes: 0,
                access_flush_interval_secs: 60,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            lfs: Default::default(),
            shutdown: Default::default(),
            max_concurrent_git_processes: 4,
            async_materialize_concurrency: 2,
            use_gitoxide: true,
        }
    }

    fn upstream_path(&self) -> PathBuf {
        self.tmp
            .path()
            .join("upstreams")
            .join(self.repo.local_bare_path())
    }

    fn work_path(&self) -> PathBuf {
        self.tmp.path().join("work")
    }

    fn init(&self) {
        fs::create_dir_all(self.upstream_path().parent().expect("parent")).expect("mkdir");
        fs::create_dir_all(self.work_path()).expect("mkdir work");
        run_git(
            self.tmp.path(),
            [
                "init",
                "--bare",
                self.upstream_path().to_str().expect("path"),
            ],
        );
        run_git(&self.work_path(), ["init"]);
        run_git(
            &self.work_path(),
            ["config", "user.email", "fuzz@example.invalid"],
        );
        run_git(&self.work_path(), ["config", "user.name", "Fuzz"]);
        fs::write(self.work_path().join("README.md"), "initial\n").expect("write");
        run_git(&self.work_path(), ["add", "README.md"]);
        run_git(&self.work_path(), ["commit", "-m", "initial"]);
        run_git(&self.work_path(), ["branch", "-M", "main"]);
        run_git(
            &self.work_path(),
            [
                "remote",
                "add",
                "origin",
                self.upstream_path().to_str().expect("path"),
            ],
        );
        run_git(&self.work_path(), ["push", "origin", "main"]);
        run_git(
            &self.upstream_path(),
            ["symbolic-ref", "HEAD", "refs/heads/main"],
        );
    }

    fn commit_and_push(&self, contents: &str) -> CommitSha {
        fs::write(self.work_path().join("README.md"), format!("{contents}\n")).expect("write");
        run_git(&self.work_path(), ["add", "README.md"]);
        run_git(&self.work_path(), ["commit", "-m", contents]);
        run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
        CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).expect("sha")
    }
}

fn run_git<I, S>(cwd: &Path, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout<I, S>(cwd: &Path, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

fn request(repo: &RepoKey, selector: Selector) -> MaterializeRequest {
    MaterializeRequest {
        repo: repo.clone(),
        selector,
        upstream_authorization: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn materializer_survives_randomized_concurrent_requests() {
    // Real git subprocesses make each op slow; keep CI defaults modest.
    let config = FuzzConfig::from_env("materializer_fuzz", 6, 10);
    let fixture = Fixture::new();
    let state = Arc::new(AppState::try_new(fixture.config()).expect("app state"));
    let materializer = Materializer::new(Arc::clone(&state));
    let repo = fixture.repo.clone();

    let initial_commit =
        CommitSha::parse(git_stdout(&fixture.work_path(), ["rev-parse", "HEAD"])).expect("sha");
    let known_commits = Arc::new(Mutex::new(vec![initial_commit]));
    let fixture = Arc::new(fixture);

    let run = async {
        let mut handles = Vec::new();
        for task in 0..config.tasks {
            let mut rng = config.task_rng(task);
            let materializer = materializer.clone();
            let state = Arc::clone(&state);
            let repo = repo.clone();
            let known_commits = Arc::clone(&known_commits);
            let fixture = Arc::clone(&fixture);
            let ops = config.ops_per_task;
            // Task 0 is the writer: it mutates the upstream while everyone
            // else materializes, maximizing stale-ref races.
            let writer = task == 0;

            handles.push(tokio::spawn(async move {
                for op in 0..ops {
                    if writer && rng.u32(..2) == 0 {
                        let fixture = Arc::clone(&fixture);
                        let label = format!("change-{task}-{op}");
                        let commit =
                            tokio::task::spawn_blocking(move || fixture.commit_and_push(&label))
                                .await
                                .expect("writer task");
                        known_commits.lock().expect("commits lock").push(commit);
                        continue;
                    }

                    match rng.u32(..100) {
                        0..=29 => {
                            let _ = materializer
                                .materialize(request(&repo, Selector::DefaultBranch))
                                .await;
                        }
                        30..=54 => {
                            let selector =
                                Selector::Branch(BranchName::parse("main").expect("branch"));
                            let _ = materializer.materialize(request(&repo, selector)).await;
                        }
                        55..=74 => {
                            let commit = {
                                let commits = known_commits.lock().expect("commits lock");
                                commits[rng.usize(..commits.len())].clone()
                            };
                            let _ = materializer
                                .materialize(request(&repo, Selector::Commit(commit)))
                                .await;
                        }
                        75..=89 => {
                            let _ = materializer
                                .resolve(request(&repo, Selector::DefaultBranch))
                                .await;
                        }
                        // Race cache invalidation against in-flight
                        // materialization; Conflict while locked is expected.
                        _ => {
                            let _ = state
                                .disk
                                .invalidate_repo(PathBuf::from(repo.local_bare_path()))
                                .await;
                        }
                    }
                }
            }));
        }

        for handle in handles {
            handle.await.expect("fuzz task must not panic");
        }
    };

    tokio::time::timeout(config.deadline, run)
        .await
        .expect("materializer fuzz deadlocked: requests did not finish before the deadline");

    // The cache must still be coherent: a fresh materialize of the latest
    // commit and the default branch must both succeed.
    let latest = known_commits
        .lock()
        .expect("commits lock")
        .last()
        .cloned()
        .expect("at least one commit");
    let response = materializer
        .materialize(request(&repo, Selector::DefaultBranch))
        .await
        .expect("default-branch materialize after fuzz");
    assert_eq!(response.commit, latest, "cache served a stale head");

    materializer
        .materialize(request(&repo, Selector::Commit(latest)))
        .await
        .expect("exact-commit materialize after fuzz");

    // All repo locks and reservations must have been released.
    let status = state.disk.status().await.expect("disk status");
    assert_eq!(status.locked_repo_count, 0, "leaked repo locks: {status:?}");
    assert_eq!(status.reserved_bytes, 0, "leaked reservations: {status:?}");
}
