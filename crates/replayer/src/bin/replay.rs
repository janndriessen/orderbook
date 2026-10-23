//! The `replay` binary: serves a recorded Hyperliquid session through the
//! order book service, so the feed shows the recorded book.
//!
//! Settings come from the environment: `REPLAY_PATH` (required, a recording
//! made by `record`), `REPLAY_SPEED` (`max`, `step`, or a positive integer
//! factor; default `1`, real time; in `step` mode each Enter on stdin plays one
//! recorded moment), `ORDERBOOK_ADDR` (default `127.0.0.1:8080`) and
//! `RUST_LOG` (log filter, default `info`). When the recording ends the
//! final book stays served until Ctrl-C. The replay starts at the first full
//! book snapshot: trades at or before it and top-of-book snapshots (best bid
//! and offer messages included) before it are dropped, and a recording without
//! a full snapshot is refused. In a recording with `l2` lines, the first full
//! snapshot is the first push all price groupings share, and every `l2Book`
//! and `bbo` line is skipped.

use std::io::BufRead;
use std::iter;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context;
use engine::Step;
use replayer::{Signal, Speed, hold, load, long_waits, pace, step_on};
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Recorded gaps longer than this are logged when the speed waits them out,
/// since the replay will sit idle.
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
        skipped_bbos = loaded.skipped_bbos,
        dropped_late_trades = loaded.dropped_late_trades,
        late_trades = loaded.late_trades,
        skipped_book_lines = loaded.skipped_book_lines,
        startup_pushes = loaded.startup_pushes,
        skipped_pushes = loaded.skipped_pushes,
        trailing_pushes = loaded.trailing_pushes,
        failed_merges = loaded.failed_merges,
        conflicting_qty = loaded.conflicting_qty,
        divergence = ?loaded.divergence,
        "recording loaded"
    );
    if loaded.incomplete_last_line {
        warn!("the last line is incomplete and was left out; is the recorder still running?");
    }
    if loaded.skipped_pushes > 0 {
        warn!(
            pushes = loaded.skipped_pushes,
            "l2 pushes without every price grouping were left out after start-up"
        );
    }
    if let Some((ts, error)) = &loaded.first_merge_error {
        warn!(
            pushes = loaded.failed_merges,
            ts,
            %error,
            "l2 pushes whose price groupings do not merge were left out (first one shown)"
        );
    }
    if loaded.dropped_late_trades > 0 {
        warn!(
            trades = loaded.dropped_late_trades,
            "trades after the last complete l2 push were left out"
        );
    }
    if loaded.skipped_error_lines > 0 {
        warn!(
            lines = loaded.skipped_error_lines,
            "the recording holds error lines from the source, which were left out"
        );
    }
    if !matches!(speed, Speed::Max | Speed::Step) {
        for (index, gap) in long_waits(&loaded.commands, LONG_WAIT) {
            warn!(index, ?gap, "long gap after this command in the recording");
        }
    }

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = engine::cancel_on_ctrl_c();
    // The source thread is parked in the flow's `next` after the last command
    // and, in step mode, between moments, where only these channels wake it,
    // so any end of the service must stop the steps and drop the release.
    let (release, held) = mpsc::channel::<()>();
    let (signals, signalled) = mpsc::channel();
    if speed == Speed::Step {
        let signals = signals.clone();
        std::thread::spawn(move || {
            for _ in std::io::stdin().lock().lines().map_while(Result::ok) {
                if signals.send(Signal::Next).is_err() {
                    return;
                }
            }
            warn!("stdin closed; stepping stopped, Ctrl-C to quit");
        });
        info!("step mode: press Enter to play the next recorded moment");
    }
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            shutdown.cancelled().await;
            let _ = signals.send(Signal::Stop);
            drop(release);
        }
    });

    // Logged when the flow itself runs out, so a stop in the middle of a
    // recording does not claim the recording finished.
    let finished = iter::once_with(|| {
        info!("replay finished, holding the final book");
        None::<Step>
    })
    .flatten();
    let paced = pace(loaded.commands, speed).chain(finished);
    let flow: Box<dyn Iterator<Item = Step> + Send> = if speed == Speed::Step {
        Box::new(step_on(paced, signalled))
    } else {
        Box::new(paced)
    };
    engine::run(listener, hold(flow, held), shutdown, engine::PING_INTERVAL).await?;
    info!("service stopped");
    Ok(())
}
