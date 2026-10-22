//! The `orderbook` binary: serves a live BTC-USD book over WebSocket.
//!
//! Settings come from the environment: `ORDERBOOK_ADDR` (default
//! `127.0.0.1:8080`), `ORDERBOOK_SEED` (optional random-walk seed) and
//! `RUST_LOG` (log filter, default `info`).

use anyhow::Context;
use engine::RandomWalk;
use protocol::Ts;
use tokio::net::TcpListener;
use tracing::info;
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

    let shutdown = engine::cancel_on_ctrl_c();

    // The same seed gives the same command stream; timestamps start at launch time.
    let start_ts = now_ts();
    let seed = seed.unwrap_or(start_ts);
    info!(seed, "starting random-walk order flow");
    let flow = RandomWalk::new(seed, start_ts);

    engine::run(listener, flow, shutdown, engine::PING_INTERVAL).await?;
    Ok(())
}

/// Current wall-clock time as venue time, used only to start a walk.
fn now_ts() -> Ts {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}
