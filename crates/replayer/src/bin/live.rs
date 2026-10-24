//! The `live` binary: serves Hyperliquid's live BTC book through the order
//! book service, so the feed shows the book as the source's web app does.
//!
//! It connects the way `record` does (seven connections, see the
//! `hyperliquid_recorder` library), assembles the `l2` lines into pushes and
//! sends the commands to the engine as they come, without pacing. When any
//! connection fails (closed, lost, 30 seconds without a full book snapshot,
//! 5 seconds without an `l2` message on one of them, or 10 seconds without a
//! push merging or a stale book being skipped) all of them are opened again
//! after a pause that starts at 1 second and doubles up to 30 seconds; the
//! engine keeps its book meanwhile and catches up with the first push of the
//! new connections. Ctrl-C stops the service and the connections.
//!
//! Settings come from the environment: `ORDERBOOK_ADDR` (default
//! `127.0.0.1:8080`), `LIVE_RECORD_PATH` (optional; also writes every line
//! received to this JSON Lines file, which `replay` can serve, and never
//! overwrites an existing file; a run that reconnected leaves a gap in it) and
//! `RUST_LOG` (log filter, default `info`).

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use anyhow::Context;
use hyperliquid_recorder::URL;
use replayer::live::{Retry, serve};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let addr = std::env::var("ORDERBOOK_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
    let record: Option<Box<dyn Write + Send>> = match std::env::var("LIVE_RECORD_PATH") {
        Ok(path) => {
            let path = PathBuf::from(path);
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            let file = File::create_new(&path).with_context(|| {
                format!("cannot create {} (it must not exist yet)", path.display())
            })?;
            info!(path = %path.display(), "recording the lines received");
            Some(Box::new(file))
        }
        Err(_) => None,
    };
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = engine::cancel_on_ctrl_c();
    serve(listener, URL, record, shutdown, Retry::default()).await?;
    info!("service stopped");
    Ok(())
}
