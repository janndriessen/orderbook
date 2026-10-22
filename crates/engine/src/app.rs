//! Wires the source, the engine, the publisher and the WebSocket server together.

use std::any::Any;
use std::io;
use std::thread::JoinHandle;
use std::time::Duration;

use protocol::MarketInfo;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::source::{self, Step};
use crate::ws;
use crate::{matcher, publisher};

/// The market the service publishes.
pub fn market() -> MarketInfo {
    MarketInfo {
        symbol: "BTC-USD".to_owned(),
        price_decimals: 0,
        qty_decimals: 5,
    }
}

/// A shutdown token that is cancelled by Ctrl-C.
///
/// Spawns the task that waits for the signal, so it must be called inside a
/// Tokio runtime. Both the signal and a failure to listen for it are logged.
pub fn cancel_on_ctrl_c() -> CancellationToken {
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
    shutdown
}

/// Runs the service on `listener` until `shutdown` fires or a component dies.
///
/// Starts the source thread with the given order `flow`, the engine thread
/// (the matcher), the publisher task and the WebSocket server, and waits for
/// all of them to stop.
///
/// Clients are pinged every `ping_interval` and dropped after a few silent
/// intervals. A zero `ping_interval` is rejected with `InvalidInput` before
/// anything starts.
///
/// Returns an error if the server failed or if the publisher, the engine or
/// the source panicked, so a supervisor can restart the process.
pub async fn run(
    listener: TcpListener,
    flow: impl Iterator<Item = Step> + Send + 'static,
    shutdown: CancellationToken,
    ping_interval: Duration,
) -> io::Result<()> {
    if ping_interval.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ping interval must be greater than zero",
        ));
    }
    let addr = listener.local_addr()?;
    let (command_queue, commands) = mpsc::channel(source::COMMAND_QUEUE_CAPACITY);
    let (event_queue, events) = mpsc::channel(matcher::EVENT_QUEUE_CAPACITY);
    // Fallible steps come before the publisher and the server start, so an
    // early return leaves neither running without a way to shut down. If the
    // source fails to start, the engine thread is already running; it exits on
    // its own once the dropped sender closes its command queue.
    let (stop_source, source_stop) = std::sync::mpsc::channel();
    let engine = matcher::spawn(commands, event_queue)?;
    let source = source::spawn(flow, command_queue, source_stop)?;
    let (handle, publisher) = publisher::spawn(market(), events, shutdown.clone());

    info!(%addr, "serving feed at /ws");
    let served = axum::serve(
        listener,
        ws::router(handle, shutdown.clone(), ping_interval),
    )
    .with_graceful_shutdown(shutdown.clone().cancelled_owned())
    .await;

    shutdown.cancel();
    // Dropping the stop sender ends the source's pause, so it stops and closes
    // the command queue, which ends the engine's receive. A source stuck on a
    // full queue stops the other way round: the publisher has dropped the
    // event receiver, so the engine's next send fails, it drops the command
    // receiver and the source's send fails in turn.
    drop(stop_source);
    let mut failed = false;
    if let Err(err) = publisher.await {
        error!(%err, "publisher task failed");
        failed = true;
    }
    if !join_threads(vec![("engine", engine), ("source", source)]).await {
        failed = true;
    }
    served?;
    if failed {
        return Err(io::Error::other("a service component failed"));
    }
    Ok(())
}

/// Waits for each named thread to end, in order, and logs any that panicked,
/// with its panic message, or could not be joined.
///
/// Returns whether every thread ended normally. A failure does not stop the
/// joining, so every thread is waited for. Joining blocks, so it runs off the
/// runtime.
async fn join_threads(threads: Vec<(&'static str, JoinHandle<()>)>) -> bool {
    let mut clean = true;
    for (name, thread) in threads {
        match tokio::task::spawn_blocking(move || thread.join()).await {
            Ok(Ok(())) => {}
            Ok(Err(payload)) => {
                let reason = panic_message(payload.as_ref());
                error!(thread = name, reason, "thread panicked");
                clean = false;
            }
            Err(err) => {
                error!(%err, thread = name, "could not join thread");
                clean = false;
            }
        }
    }
    clean
}

/// The message a panic was raised with: a `panic!` with only a literal carries
/// a `&str`, one with format arguments a `String`.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_panic_message_is_read_from_either_payload() {
        let literal = thread::spawn(|| panic!("literal")).join();
        // A runtime argument: constant ones are folded into a `&str` payload.
        let seq = std::hint::black_box(7);
        let formatted = thread::spawn(move || panic!("formatted {seq}")).join();
        let other = thread::spawn(|| std::panic::panic_any(7_u8)).join();
        for (payload, message) in [
            (literal, "literal"),
            (formatted, "formatted 7"),
            (other, "no message"),
        ] {
            let payload = payload.expect_err("the thread panics");
            assert_eq!(panic_message(payload.as_ref()), message);
        }
    }

    #[tokio::test]
    async fn threads_that_end_normally_join_cleanly() {
        let threads = vec![
            ("first", thread::spawn(|| {})),
            ("second", thread::spawn(|| {})),
        ];
        assert!(join_threads(threads).await);
    }

    #[tokio::test]
    async fn a_panicked_thread_is_reported_and_the_rest_are_still_joined() {
        let finished = Arc::new(AtomicBool::new(false));
        let slow = {
            let finished = Arc::clone(&finished);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                finished.store(true, Ordering::SeqCst);
            })
        };
        let threads = vec![
            (
                "panicked",
                thread::spawn(|| panic!("deliberate panic in a test thread")),
            ),
            ("slow", slow),
        ];
        assert!(!join_threads(threads).await);
        assert!(
            finished.load(Ordering::SeqCst),
            "the thread after the panicked one was joined"
        );
    }
}
