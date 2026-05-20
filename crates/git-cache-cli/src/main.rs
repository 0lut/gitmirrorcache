use clap::{Parser, Subcommand};
use git_cache_core::AppConfig;
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
    /// Placeholder for warming a repo/ref into cache.
    Warm { repo: String, selector: String },
    /// Remove expired sessions from disk and object store.
    SessionCleanup,
}

fn main() -> anyhow::Result<()> {
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
            println!("warm is scaffolded; requested repo `{repo}` with selector `{selector}`");
        }
        Command::SessionCleanup => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let config = AppConfig::from_env()?;
                let state = Arc::new(AppState::try_new(config)?);
                let materializer = Materializer::new(state);
                let report = materializer.cleanup_expired_sessions().await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                Ok::<_, anyhow::Error>(())
            })?;
        }
    }

    Ok(())
}
