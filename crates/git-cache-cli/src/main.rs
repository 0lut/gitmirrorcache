use clap::{Parser, Subcommand};
use git_cache_core::{AppConfig, BranchName, CommitSha, RepoKey, RequestMode, Selector};
use git_cache_disk::DiskManager;
use git_cache_domain::{AppState, Materializer};
use git_cache_objectstore::read_ref_manifest;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Parser)]
#[command(name = "git-cache")]
#[command(about = "Admin CLI for the Git fetch cache")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the effective environment-backed config.
    Config,
    /// Show local cache disk accounting.
    DiskStatus,
    /// Warm a repo/ref into the cache by materializing it.
    Warm {
        /// Repository key (e.g. github.com/owner/repo)
        repo: String,
        /// Selector: "default", a branch name, or a 40-char commit SHA
        selector: String,
    },
    /// Inspect cached manifests for a repository.
    Inspect {
        /// Repository key (e.g. github.com/owner/repo)
        repo: String,
    },
    /// Remove stale temporary files from the disk cache.
    Prune {
        #[arg(long, default_value = "3600")]
        older_than_secs: u64,
    },
    /// Run integrity checks and repair a cached repository.
    Repair {
        /// Repository key (e.g. github.com/owner/repo)
        repo: String,
    },
}

fn parse_selector(s: &str) -> anyhow::Result<Selector> {
    if s == "default" {
        return Ok(Selector::DefaultBranch);
    }
    if let Ok(sha) = CommitSha::parse(s) {
        return Ok(Selector::Commit(sha));
    }
    let branch = BranchName::parse(s)?;
    Ok(Selector::Branch(branch))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Config => {
            let config = AppConfig::from_env()?;
            println!("{}", serde_json::to_string_pretty(&config)?);
        }
        Command::DiskStatus => {
            let config = AppConfig::from_env()?;
            let manager = DiskManager::new(
                config.cache_root,
                config.disk.quota_bytes,
                config.disk.min_free_bytes,
            );
            println!("{}", serde_json::to_string_pretty(&manager.status()?)?);
        }
        Command::Warm { repo, selector } => {
            let repo = RepoKey::parse(repo)?;
            let selector = parse_selector(&selector)?;
            let config = AppConfig::from_env()?;
            let state = Arc::new(AppState::try_new(config)?);
            let materializer = Materializer::new(state);
            let response = materializer
                .materialize(git_cache_core::MaterializeRequest {
                    repo,
                    selector,
                    mode: RequestMode::Strict,
                })
                .await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
        Command::Inspect { repo } => {
            let repo = RepoKey::parse(repo)?;
            let config = AppConfig::from_env()?;
            let state = AppState::try_new(config)?;
            let report = inspect_repo(&state, &repo).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Prune { older_than_secs } => {
            let config = AppConfig::from_env()?;
            let manager = DiskManager::new(
                config.cache_root,
                config.disk.quota_bytes,
                config.disk.min_free_bytes,
            );
            let report = manager.cleanup_stale_temps(Duration::from_secs(older_than_secs))?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Repair { repo } => {
            let repo = RepoKey::parse(repo)?;
            let config = AppConfig::from_env()?;
            let state = AppState::try_new(config)?;
            let report = repair_repo(&state, &repo).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}

#[derive(Serialize)]
struct InspectReport {
    repo: String,
    default_branch: Option<serde_json::Value>,
    disk_index: Option<serde_json::Value>,
}

async fn inspect_repo(state: &AppState, repo: &RepoKey) -> anyhow::Result<InspectReport> {
    let default_ref_key = format!("repos/{repo}/manifests/refs/heads/HEAD.json");
    let default_manifest = match state.store.get(&default_ref_key).await? {
        Some(data) => serde_json::from_slice::<serde_json::Value>(&data).ok(),
        None => {
            let ref_manifest = read_ref_manifest(&*state.store, repo, "refs/heads/main").await?;
            ref_manifest.map(|m| serde_json::to_value(m).unwrap_or_default())
        }
    };

    let bare_path = std::path::PathBuf::from(repo.local_bare_path());
    let disk_index = state
        .disk
        .inner()
        .repo_index()
        .ok()
        .and_then(|idx| idx.repos.get(&bare_path).cloned())
        .map(|entry| serde_json::to_value(entry).unwrap_or_default());

    Ok(InspectReport {
        repo: repo.to_string(),
        default_branch: default_manifest,
        disk_index,
    })
}

#[derive(Serialize)]
struct RepairReport {
    repo: String,
    fsck_ok: bool,
    fsck_output: String,
    disk_index_synced: bool,
}

async fn repair_repo(state: &AppState, repo: &RepoKey) -> anyhow::Result<RepairReport> {
    let repo_dir = state
        .config
        .cache_root
        .join("repos")
        .join(repo.local_bare_path());

    let (fsck_ok, fsck_output) = if repo_dir.exists() {
        match state.git.fsck(&repo_dir).await {
            Ok(output) => (true, String::from_utf8_lossy(&output.stdout).to_string()),
            Err(err) => (false, err.to_string()),
        }
    } else {
        (false, format!("repo directory does not exist: {}", repo_dir.display()))
    };

    let disk_index_synced = if repo_dir.exists() {
        state
            .disk
            .inner()
            .record_repo_access(repo.local_bare_path())
            .is_ok()
    } else {
        false
    };

    Ok(RepairReport {
        repo: repo.to_string(),
        fsck_ok,
        fsck_output,
        disk_index_synced,
    })
}

#[cfg(test)]
mod tests {
    use assert_cmd::Command;

    #[test]
    fn cli_help_does_not_panic() {
        Command::cargo_bin("git-cache")
            .unwrap()
            .arg("--help")
            .assert()
            .success();
    }

    #[test]
    fn warm_help_does_not_panic() {
        Command::cargo_bin("git-cache")
            .unwrap()
            .args(["warm", "--help"])
            .assert()
            .success();
    }

    #[test]
    fn inspect_help_does_not_panic() {
        Command::cargo_bin("git-cache")
            .unwrap()
            .args(["inspect", "--help"])
            .assert()
            .success();
    }

    #[test]
    fn prune_help_does_not_panic() {
        Command::cargo_bin("git-cache")
            .unwrap()
            .args(["prune", "--help"])
            .assert()
            .success();
    }

    #[test]
    fn repair_help_does_not_panic() {
        Command::cargo_bin("git-cache")
            .unwrap()
            .args(["repair", "--help"])
            .assert()
            .success();
    }
}
