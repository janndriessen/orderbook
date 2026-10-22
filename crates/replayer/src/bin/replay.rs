//! The `replay` binary: serves a recorded Hyperliquid session through the
//! order book service, so the feed shows the recorded book.
//!
//! Settings come from the environment: `REPLAY_PATH` (required, a recording
//! made by `record`), `REPLAY_SPEED` (`max`, or a positive integer factor;
//! default `1`, real time), `ORDERBOOK_ADDR` (default `127.0.0.1:8080`) and
//! `RUST_LOG` (log filter, default `info`). When the recording ends the
//! final book stays served until Ctrl-C. The replay starts at the first full
//! book snapshot: trades at or before it and top-of-book snapshots before it
//! are dropped, and a recording without a full snapshot is refused.

use std::time::Duration;

use anyhow::Context;
use replayer::{Speed, hold, load, long_waits, pace};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Recorded gaps longer than this are logged, since the replay will sit idle.
const LONG_WAIT: Duration = Duration::from_secs(60);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = std::env::var("REPLAY_PATH").context("REPLAY_PATH must name a recording")?;
    let speed = std::env::var("REPLAY_SPEED")
        .unwrap_or_else(|_| "1".to_owned())
        .parse::<Speed>()
        .context("REPLAY_SPEED")?;
    let addr = std::env::var("ORDERBOOK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());

    let recording =
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {path}"))?;
    let loaded =
        load(&recording, &engine::market()).with_context(|| format!("cannot load {path}"))?;
    info!(
        commands = loaded.commands.len(),
        dropped_trades = loaded.dropped_trades,
        dropped_top_snapshots = loaded.dropped_top_snapshots,
        divergence = ?loaded.divergence,
        "recording loaded"
    );
    if loaded.incomplete_last_line {
        warn!("the last line is incomplete and was left out; is the recorder still running?");
    }
    for (index, gap) in long_waits(&loaded.commands, LONG_WAIT) {
        warn!(index, ?gap, "long gap after this command in the recording");
    }

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = engine::cancel_on_ctrl_c();
    // The source thread is parked in the flow's `next` after the last command,
    // where only the release wakes it, so any end of the service must send it.
    let (release, held) = std::sync::mpsc::channel::<()>();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown.cancelled().await;
            drop(release);
        }
    });

    engine::run(
        listener,
        hold(pace(loaded.commands, speed), held),
        shutdown,
        engine::PING_INTERVAL,
    )
    .await?;
    info!("service stopped");
    Ok(())
}
