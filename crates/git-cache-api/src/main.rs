use git_cache_core::AppConfig;
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = AppConfig::from_env()?;
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(addr = %config.bind_addr, "starting git-cache-api");

    git_cache_api::serve(listener, config).await?;
    Ok(())
}
