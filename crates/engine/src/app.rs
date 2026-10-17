//! Wires the source, the publisher and the WebSocket server together.

use std::io;

use protocol::MarketInfo;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::source::{self, RandomWalk};
use crate::{publisher, ws};

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
/// Starts the random-walk source on its own thread, the publisher task and
/// the WebSocket server, and waits for all of them to stop. Without a `seed`
/// one is derived from the clock.
///
/// Returns an error if the server failed or if the publisher or the source
/// panicked, so a supervisor can restart the process.
pub async fn run(
    listener: TcpListener,
    seed: Option<u64>,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let addr = listener.local_addr()?;
    let start_ts = source::now_ts();
    let seed = seed.unwrap_or(start_ts);
    let (queue, events) = mpsc::channel(source::QUEUE_CAPACITY);
    // Fallible steps come first so that an early return leaves nothing running.
    let source = source::spawn(RandomWalk::new(seed, start_ts), queue)?;
    let (handle, publisher) = publisher::spawn(market(), events, shutdown.clone());

    // The same seed gives the same event stream; timestamps start at launch time.
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
    // The publisher dropped the queue's receiver, so the source thread exits
    // on its next send; joining blocks briefly, so keep it off the runtime.
    match tokio::task::spawn_blocking(move || source.join()).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            error!("source thread panicked");
            failed = true;
        }
        Err(err) => {
            error!(%err, "could not join source thread");
            failed = true;
        }
    }
    served?;
    if failed {
        return Err(io::Error::other("a service component failed"));
    }
    Ok(())
}
