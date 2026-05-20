use clap::{Parser, Subcommand};
use git_cache_core::AppConfig;
use git_cache_disk::DiskManager;

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
    }

    Ok(())
}
