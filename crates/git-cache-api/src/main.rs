use git_cache_api::{app_with_shutdown_async, ReadinessGate};
use git_cache_core::AppConfig;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = AppConfig::from_env()?;
    let shutdown_config = config.shutdown.clone();
    let listener = TcpListener::bind(config.bind_addr).await?;
    info!(addr = %config.bind_addr, "starting git-cache-api");

    let (app, readiness) = app_with_shutdown_async(config).await?;

    let readiness_delay = Duration::from_secs(shutdown_config.readiness_delay_seconds);
    let drain_timeout = Duration::from_secs(shutdown_config.drain_timeout_seconds);
    let (drain_deadline_tx, drain_deadline_rx) = tokio::sync::oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(graceful_shutdown(
        readiness,
        readiness_delay,
        drain_timeout,
        drain_deadline_tx,
    ));

    tokio::select! {
        result = server => result?,
        _ = wait_for_drain_deadline(drain_deadline_rx, drain_timeout) => {
            warn!(
                drain_timeout_seconds = drain_timeout.as_secs(),
                "drain timeout elapsed with requests still in flight; exiting"
            );
        }
    }
    Ok(())
}

/// Resolves once the process should stop accepting new connections: after a
/// SIGTERM/SIGINT is received, readiness is failed, and the configured
/// readiness propagation delay has passed. Signals `drain_deadline_tx` so the
/// caller can bound the remaining in-flight drain.
async fn graceful_shutdown(
    readiness: ReadinessGate,
    readiness_delay: Duration,
    drain_timeout: Duration,
    drain_deadline_tx: tokio::sync::oneshot::Sender<()>,
) {
    shutdown_signal().await;
    readiness.begin_shutdown();
    info!(
        readiness_delay_seconds = readiness_delay.as_secs(),
        drain_timeout_seconds = drain_timeout.as_secs(),
        "shutdown signal received; failing readiness, then draining in-flight requests"
    );
    tokio::time::sleep(readiness_delay).await;
    let _ = drain_deadline_tx.send(());
}

async fn wait_for_drain_deadline(
    drain_started: tokio::sync::oneshot::Receiver<()>,
    drain_timeout: Duration,
) {
    if drain_started.await.is_err() {
        // Server finished before shutdown began; never force-exit.
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(drain_timeout).await;
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!(%err, "failed to install SIGINT handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                warn!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
