//! The `record` binary: writes the live BTC book and trades from Hyperliquid's
//! public WebSocket to a JSON Lines file, one received message per line.
//!
//! The connections, the feeds they subscribe to, the removal of `users` fields
//! (wallet addresses) and the tagging of `l2` lines are described on the
//! `hyperliquid_recorder` library. The run ends with an error, and there is no
//! reconnect because a gap in a recording would be silent, when any connection
//! is closed or lost, a message is not valid JSON (it is never written), or a
//! feed stays silent (30 seconds without a full book snapshot, 5 seconds
//! without an `l2` message).
//!
//! Settings come from the environment: `RECORD_PATH` (default
//! `data/hyperliquid-btc-<unix seconds>.jsonl`; an existing file is never
//! overwritten), `RECORD_SECONDS` (optional, above zero; stop after that long,
//! otherwise run until Ctrl-C) and `RUST_LOG` (log filter, default `info`).

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hyperliquid_recorder::{Connections, URL};
use tokio::time::sleep;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Writes messages from the connections into `out` until the time limit or
/// Ctrl-C; any other end is an error.
async fn record(
    mut connections: Connections,
    out: &mut BufWriter<File>,
    limit: Option<Duration>,
) -> anyhow::Result<()> {
    let stop = sleep(limit.unwrap_or(Duration::MAX));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(stop, ctrl_c);
    loop {
        tokio::select! {
            () = &mut stop => return Ok(()),
            signal = &mut ctrl_c => {
                signal.context("cannot listen for ctrl-c")?;
                return Ok(());
            }
            line = connections.next() => {
                serde_json::to_writer(&mut *out, &line?)?;
                out.write_all(b"\n")?;
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let path = match std::env::var("RECORD_PATH") {
        Ok(path) => PathBuf::from(path),
        Err(_) => {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            PathBuf::from(format!("data/hyperliquid-btc-{secs}.jsonl"))
        }
    };
    let limit = match std::env::var("RECORD_SECONDS") {
        Ok(value) => {
            let seconds = value.parse::<u64>().ok().filter(|seconds| *seconds > 0);
            Some(Duration::from_secs(
                seconds.context("RECORD_SECONDS must be an integer above zero")?,
            ))
        }
        Err(_) => None,
    };

    let connections = Connections::connect(URL).await?;

    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let file = File::create_new(&path)
        .with_context(|| format!("cannot create {} (it must not exist yet)", path.display()))?;
    let mut out = BufWriter::new(file);
    info!(path = %path.display(), "recording");

    let result = record(connections, &mut out, limit).await;
    let flushed = out.flush().context("cannot flush the recording");
    result?;
    flushed?;
    info!("stopped");
    Ok(())
}
