use super::generations::{bundle_key, pending_generation_from_key, push_unique_commit};
use super::*;
#[cfg(feature = "s3-tests")]
use aws_credential_types::Credentials;
#[cfg(feature = "s3-tests")]
use aws_sdk_s3::config::BehaviorVersion;
#[cfg(feature = "s3-tests")]
use aws_sdk_s3::config::RequestChecksumCalculation;
#[cfg(feature = "s3-tests")]
use aws_sdk_s3::Client;
#[cfg(feature = "s3-tests")]
use aws_types::region::Region;
use git_cache_core::{AppConfig, ObjectStoreConfig};
#[cfg(feature = "s3-tests")]
use git_cache_disk::{AsyncDiskManager, DiskManager};
#[cfg(feature = "s3-tests")]
use git_cache_git::Git;
use git_cache_objectstore::read_ref_manifest;
#[cfg(feature = "s3-tests")]
use git_cache_objectstore::{ObjectStore, S3ObjectStore};
use std::fs as stdfs;
use std::net::SocketAddr;
use std::process::Command;
use tempfile::TempDir;

fn ref_manifest_key(repo: &RepoKey, branch: &str) -> String {
    git_cache_objectstore::ref_manifest_key(repo, &format!("refs/heads/{branch}"))
        .expect("validated branch ref")
}

async fn generation_manifest_for(
    state: &AppState,
    repo: &RepoKey,
    generation: GenerationId,
) -> GenerationManifest {
    read_generation_manifest(&*state.store, repo, generation)
        .await
        .unwrap()
        .unwrap()
}

async fn generation_object_keys(state: &AppState, repo: &RepoKey) -> Vec<String> {
    state
        .store
        .list_prefix(&format!("repos/{repo}/generations/"), None)
        .await
        .unwrap()
}

async fn wait_for_verified_generation(state: &AppState, repo: &RepoKey, generation: GenerationId) {
    for _ in 0..100 {
        if read_verified_generation_manifest(&*state.store, repo, generation)
            .await
            .unwrap()
            .is_some()
        {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("verified generation manifest `{generation}` not written");
}

async fn wait_for_commit_manifest(
    state: &AppState,
    repo: &RepoKey,
    commit: &CommitSha,
) -> CommitManifest {
    for _ in 0..100 {
        if let Some(manifest) = read_commit_manifest(&*state.store, repo, commit)
            .await
            .unwrap()
        {
            return manifest;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("commit manifest `{commit}` not written");
}

async fn wait_for_generation_head(
    state: &AppState,
    repo: &RepoKey,
    generation: GenerationId,
) -> RepoGenerationHead {
    for _ in 0..100 {
        if let Some(head) = read_repo_generation_head(&*state.store, repo)
            .await
            .unwrap()
        {
            if head.generation == generation {
                return head;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("generation head `{generation}` not written");
}

pub struct GitFixture {
    pub tmp: TempDir,
    pub repo: RepoKey,
}

impl GitFixture {
    pub fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let repo = RepoKey::parse("github.com/org/repo").unwrap();
        let fixture = Self { tmp, repo };
        fixture.init();
        fixture
    }

    pub fn state_config(&self) -> AppConfig {
        AppConfig {
            bind_addr: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
            cache_root: self.cache_root(),
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
                quota_bytes: 1024 * 1024 * 1024,
                min_free_bytes: 0,
            },
            git_remote: Default::default(),
            compaction: Default::default(),
            max_concurrent_git_processes: git_cache_core::default_max_concurrent_git_processes(),
            max_concurrent_generation_verifications: 1,
        }
    }

    pub fn state(&self) -> AppState {
        AppState::try_new(self.state_config()).unwrap()
    }

    #[cfg(feature = "s3-tests")]
    pub fn state_with_store(&self, store: Arc<dyn ObjectStore>) -> AppState {
        let config = self.state_config();
        let git = Git::with_concurrency_limit(
            config.git_binary.clone(),
            std::time::Duration::from_secs(config.git_timeout_seconds),
            config.max_concurrent_git_processes,
        )
        .with_output_limit(config.max_git_output_bytes);
        let disk = DiskManager::new(
            &config.cache_root,
            config.disk.quota_bytes,
            config.disk.min_free_bytes,
        );
        AppState {
            config,
            store,
            git,
            disk: AsyncDiskManager::new(disk),
            generation_verification_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub fn cache_root(&self) -> PathBuf {
        self.tmp.path().join("cache")
    }

    pub fn work_path(&self) -> PathBuf {
        self.tmp.path().join("work")
    }

    pub fn upstream_path(&self) -> PathBuf {
        self.tmp
            .path()
            .join("upstreams")
            .join(self.repo.local_bare_path())
    }

    fn init(&self) {
        stdfs::create_dir_all(self.upstream_path().parent().unwrap()).unwrap();
        stdfs::create_dir_all(self.work_path()).unwrap();
        run_git(
            self.tmp.path(),
            ["init", "--bare", self.upstream_path().to_str().unwrap()],
        );
        run_git(&self.work_path(), ["init"]);
        run_git(
            &self.work_path(),
            ["config", "user.email", "cache@example.invalid"],
        );
        run_git(&self.work_path(), ["config", "user.name", "Cache Test"]);
        stdfs::write(self.work_path().join("README.md"), "initial\n").unwrap();
        run_git(&self.work_path(), ["add", "README.md"]);
        run_git(&self.work_path(), ["commit", "-m", "initial"]);
        run_git(&self.work_path(), ["branch", "-M", "main"]);
        run_git(
            &self.work_path(),
            [
                "remote",
                "add",
                "origin",
                self.upstream_path().to_str().unwrap(),
            ],
        );
        run_git(&self.work_path(), ["push", "origin", "main"]);
        run_git(
            &self.upstream_path(),
            ["symbolic-ref", "HEAD", "refs/heads/main"],
        );
    }

    pub fn commit_and_push(&self, contents: &str) -> CommitSha {
        stdfs::write(self.work_path().join("README.md"), format!("{contents}\n")).unwrap();
        run_git(&self.work_path(), ["add", "README.md"]);
        run_git(&self.work_path(), ["commit", "-m", contents]);
        run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
        CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
    }

    pub fn push_head_to_branch(&self, branch: &str) {
        run_git(
            &self.work_path(),
            ["push", "--force", "origin", &format!("HEAD:{branch}")],
        );
    }

    pub fn replace_history_and_push(&self, contents: &str) -> CommitSha {
        run_git(&self.work_path(), ["checkout", "--orphan", "replacement"]);
        stdfs::write(self.work_path().join("README.md"), format!("{contents}\n")).unwrap();
        run_git(&self.work_path(), ["add", "README.md"]);
        run_git(&self.work_path(), ["commit", "-m", contents]);
        run_git(&self.work_path(), ["branch", "-M", "main"]);
        run_git(&self.work_path(), ["push", "--force", "origin", "main"]);
        CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
    }

    pub fn head_commit(&self) -> CommitSha {
        CommitSha::parse(git_stdout(&self.work_path(), ["rev-parse", "HEAD"])).unwrap()
    }
}

#[cfg(feature = "s3-tests")]
struct MinioFixture {
    store: Arc<dyn ObjectStore>,
}

#[cfg(feature = "s3-tests")]
impl MinioFixture {
    async fn new() -> Option<Self> {
        if std::env::var("GIT_CACHE_S3_INTEGRATION").ok().as_deref() != Some("1") {
            return None;
        }

        let endpoint = std::env::var("GIT_CACHE_S3_ENDPOINT")
            .unwrap_or_else(|_| "http://127.0.0.1:9000".into());
        let bucket =
            std::env::var("GIT_CACHE_S3_BUCKET").unwrap_or_else(|_| "gitmirrorcache-test".into());
        let access_key =
            std::env::var("GIT_CACHE_S3_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".into());
        let secret_key =
            std::env::var("GIT_CACHE_S3_SECRET_KEY").unwrap_or_else(|_| "minioadmin".into());
        let prefix = format!("domain-tests/{}", uuid::Uuid::now_v7());
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(endpoint)
            .force_path_style(true)
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .credentials_provider(Credentials::new(
                access_key,
                secret_key,
                None,
                None,
                "minio-integration",
            ))
            .build();
        let client = Client::from_conf(config);
        client.create_bucket().bucket(&bucket).send().await.ok();
        let store = S3ObjectStore::new(client, bucket, prefix).unwrap();
        Some(Self {
            store: Arc::new(store),
        })
    }
}

fn short_prefix_not_matching(commit: &CommitSha, other: &CommitSha) -> ShortCommitSha {
    let length = (8..40)
        .find(|length| commit.as_str()[..*length] != other.as_str()[..*length])
        .unwrap();
    ShortCommitSha::parse(&commit.as_str()[..length]).unwrap()
}

fn run_git<I, S>(cwd: &FsPath, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout<I, S>(cwd: &FsPath, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn make_comparison(refs: &[(&str, &str)], default_branch: Option<&str>) -> UpstreamRefComparison {
    UpstreamRefComparison {
        default_branch: default_branch.map(|s| s.to_string()),
        all_upstream: refs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    }
}

fn parse_pkt_lines(data: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let hex = std::str::from_utf8(&data[offset..offset + 4]).unwrap();
        let len = usize::from_str_radix(hex, 16).unwrap();
        if len == 0 {
            offset += 4;
            continue;
        }
        assert!(len >= 4);
        assert!(offset + len <= data.len());
        lines.push(data[offset + 4..offset + len].to_vec());
        offset += len;
    }
    lines
}

mod contention_tests;
mod direct_git_tests;
mod generation_tests;
mod key_path_tests;
#[cfg(feature = "s3-tests")]
mod minio_tests;
mod performance_tests;
mod selector_tests;
