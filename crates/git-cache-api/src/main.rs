use git_cache_api::app_with_shutdown;
use git_cache_core::AppConfig;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = AppConfig::from_env()?;
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(addr = %config.bind_addr, "starting git-cache-api");

    let (router, shutdown_handle) = app_with_shutdown(config).await?;

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("shutting down — releasing leases");
    shutdown_handle.shutdown().await;
    info!("shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install ctrl+c handler");
    };

    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
