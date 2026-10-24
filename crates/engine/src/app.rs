//! Wires the source, the engine, the publisher and the WebSocket server together.

use std::any::Any;
use std::ffi::OsStr;
use std::io;
use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::Duration;

use axum::serve::ListenerExt;
use protocol::MarketInfo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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

/// The client cap from `ORDERBOOK_MAX_CLIENTS`, or the default when unset.
///
/// Fails with `InvalidInput` when the value is not a non-negative integer in
/// UTF-8; [`run`] checks the range.
pub fn max_clients_from_env() -> io::Result<usize> {
    parse_max_clients(std::env::var_os("ORDERBOOK_MAX_CLIENTS").as_deref())
}

fn parse_max_clients(value: Option<&OsStr>) -> io::Result<usize> {
    let Some(value) = value else {
        return Ok(ws::MAX_CLIENTS);
    };
    value
        .to_str()
        .and_then(|text| text.parse::<usize>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("ORDERBOOK_MAX_CLIENTS must be an integer, got {value:?}"),
            )
        })
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
/// At most `max_clients` feed clients are served at once (see
/// [`max_clients_from_env`]); zero, or more than [`ws::MAX_CLIENTS_LIMIT`], is
/// rejected like a zero ping interval.
///
/// Returns an error if the server failed or if the publisher, the engine or
/// the source panicked, so a supervisor can restart the process.
pub async fn run(
    listener: TcpListener,
    flow: impl Iterator<Item = Step> + Send + 'static,
    shutdown: CancellationToken,
    ping_interval: Duration,
    max_clients: usize,
) -> io::Result<()> {
    if !(1..=ws::MAX_CLIENTS_LIMIT).contains(&max_clients) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "client limit must be between 1 and {}, got {max_clients}",
                ws::MAX_CLIENTS_LIMIT
            ),
        ));
    }
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
        feed_listener(listener),
        ws::router(handle, shutdown.clone(), ping_interval, max_clients),
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

/// Wraps `listener` so every accepted connection has Nagle's algorithm off.
///
/// A feed frame is small and time-sensitive: with Nagle on, the kernel holds a
/// write back until the previous one is acknowledged, delaying the update by
/// up to a round trip. `axum::serve` leaves the option at the OS default.
fn feed_listener(
    listener: TcpListener,
) -> impl axum::serve::Listener<Io = TcpStream, Addr = SocketAddr> {
    listener.tap_io(|tcp| {
        if let Err(err) = tcp.set_nodelay(true) {
            warn!(%err, "cannot disable Nagle's algorithm on a feed connection");
        }
    })
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

    use axum::serve::Listener;

    use super::*;

    #[test]
    fn the_client_cap_is_parsed_from_the_environment_value() {
        let parse = |text: &str| parse_max_clients(Some(OsStr::new(text)));
        assert_eq!(parse_max_clients(None).expect("default"), ws::MAX_CLIENTS);
        assert_eq!(parse("42").expect("number"), 42);
        for bad in ["", "abc", "-1", "1.5", " 7"] {
            let err = parse(bad).expect_err(bad);
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_client_cap_is_an_error_not_the_default() {
        use std::os::unix::ffi::OsStrExt;
        let err = parse_max_clients(Some(OsStr::from_bytes(b"5\xff"))).expect_err("not UTF-8");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn accepted_feed_connections_have_nagle_off() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let mut listener = feed_listener(listener);
        let _client = TcpStream::connect(addr).await.expect("connect");
        let (accepted, _) = listener.accept().await;
        assert!(accepted.nodelay().expect("read nodelay"));
    }

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
