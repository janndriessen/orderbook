//! Wires the source, the engine, the publisher and the WebSocket server together.

use std::io;

use protocol::MarketInfo;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::source::{self, RandomWalk};
use crate::{matcher, publisher, ws};

/// The market the service publishes.
pub fn market() -> MarketInfo {
    MarketInfo {
        symbol: "BTC-USD".to_owned(),
        price_decimals: 0,
        qty_decimals: 5,
    }
}

/// Runs the service on `listener` until `shutdown` fires or a component dies.
///
/// Starts the source thread (random-walk order flow), the engine thread (the
/// matcher), the publisher task and the WebSocket server, and waits for all of
/// them to stop. Without a `seed` one is derived from the clock.
///
/// Returns an error if the server failed or if the publisher, the engine or
/// the source panicked, so a supervisor can restart the process.
pub async fn run(
    listener: TcpListener,
    seed: Option<u64>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    let start_ts = source::now_ts();
    let seed = seed.unwrap_or(start_ts);
    let (command_queue, commands) = mpsc::channel(source::COMMAND_QUEUE_CAPACITY);
    let (event_queue, events) = mpsc::channel(matcher::EVENT_QUEUE_CAPACITY);
    // Fallible steps come before the publisher and the server start, so an
    // early return leaves neither running without a way to shut down. If the
    // source fails to start, the engine thread is already running; it exits on
    // its own once the dropped sender closes its command queue.
    let engine = matcher::spawn(commands, event_queue)?;
    let source = source::spawn(RandomWalk::new(seed, start_ts), command_queue)?;
    let (handle, publisher) = publisher::spawn(market(), events, shutdown.clone());

    // The same seed gives the same command stream; timestamps start at launch time.
    info!(%addr, seed, "serving feed at /ws");
    let served = axum::serve(listener, ws::router(handle, shutdown.clone()))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;

    shutdown.cancel();
    let mut failed = false;
    if let Err(err) = publisher.await {
        error!(%err, "publisher task failed");
        failed = true;
    }
    // The publisher dropped the event receiver, so the engine thread exits on
    // its next send, which drops the command receiver and stops the source in
    // turn. Joining blocks briefly, so keep it off the runtime.
    for (name, thread) in [("engine", engine), ("source", source)] {
        match tokio::task::spawn_blocking(move || thread.join()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                error!(thread = name, "thread panicked");
                failed = true;
            }
            Err(err) => {
                error!(%err, thread = name, "could not join thread");
                failed = true;
            }
        }
    }
    served?;
    if failed {
        return Err(io::Error::other("a service component failed"));
    }
    Ok(())
}
