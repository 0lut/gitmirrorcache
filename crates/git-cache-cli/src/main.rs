use clap::{Parser, Subcommand, ValueEnum};
use git_cache_core::{AppConfig, BranchName, CommitSha, MaterializeRequest, RequestMode, Selector, ShortCommitSha};
use git_cache_disk::DiskManager;
use git_cache_domain::{AppState, Materializer};
use std::sync::Arc;

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
    /// Warm a repo+ref into the local cache.
    Warm {
        /// Repository key, e.g. github.com/org/repo
        repo: String,
        /// Selector: a branch name, commit SHA, short commit, or "HEAD" for default branch
        selector: String,
        /// Request mode
        #[arg(long, default_value = "strict")]
        mode: CliRequestMode,
    },
    /// Remove expired sessions from disk and object store.
    SessionCleanup,
}

#[derive(Debug, Clone, ValueEnum)]
enum CliRequestMode {
    Strict,
    Cached,
}

impl From<CliRequestMode> for RequestMode {
    fn from(mode: CliRequestMode) -> Self {
        match mode {
            CliRequestMode::Strict => RequestMode::Strict,
            CliRequestMode::Cached => RequestMode::Cached,
        }
    }
}

fn parse_selector(value: &str) -> Result<Selector, git_cache_core::GitCacheError> {
    if value.eq_ignore_ascii_case("HEAD") || value.eq_ignore_ascii_case("default") {
        return Ok(Selector::DefaultBranch);
    }

    if let Ok(commit) = CommitSha::parse(value) {
        return Ok(Selector::Commit(commit));
    }

    if let Ok(short) = ShortCommitSha::parse(value) {
        if value.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Selector::ShortCommit(short));
        }
    }

    Ok(Selector::Branch(BranchName::parse(value)?))
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
        Command::Warm {
            repo,
            selector,
            mode,
        } => {
            let repo = git_cache_core::RepoKey::parse(&repo)?;
            let selector = parse_selector(&selector)?;
            let config = AppConfig::from_env()?;
            let state = Arc::new(AppState::try_new(config)?);
            let materializer = Materializer::new(state);
            let response = materializer
                .materialize(MaterializeRequest {
                    repo,
                    selector,
                    mode: mode.into(),
                })
                .await;
            match response {
                Ok(response) => {
                    println!("{}", serde_json::to_string_pretty(&response)?);
                }
                Err(error) => {
                    eprintln!("error: {error}");
                    std::process::exit(1);
                }
            }
        }
        Command::SessionCleanup => {
            let config = AppConfig::from_env()?;
            let state = Arc::new(AppState::try_new(config)?);
            let materializer = Materializer::new(state);
            let report = materializer.cleanup_expired_sessions().await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }

    Ok(())
}
