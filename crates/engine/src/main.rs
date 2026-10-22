//! The `orderbook` binary: serves a live BTC-USD book over WebSocket.
//!
//! Settings come from the environment: `ORDERBOOK_ADDR` (default
//! `127.0.0.1:8080`), `ORDERBOOK_SEED` (optional random-walk seed) and
//! `RUST_LOG` (log filter, default `info`).

use anyhow::Context;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = std::env::var("ORDERBOOK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let seed = std::env::var("ORDERBOOK_SEED")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .context("ORDERBOOK_SEED must be an unsigned integer")?;
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;

    let shutdown = CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            match tokio::signal::ctrl_c().await {
                Ok(()) => info!("shutting down"),
                Err(err) => error!(%err, "cannot listen for ctrl-c, shutting down"),
            }
            shutdown.cancel();
        }
    });

    engine::run(listener, seed, shutdown, engine::PING_INTERVAL).await?;
    Ok(())
}
