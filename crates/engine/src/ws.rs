//! WebSocket endpoint serving the market-data feed.
//!
//! Slow-consumer policy: a client that cannot keep up is disconnected, never
//! buffered without bound and never allowed to hold back the others. Two
//! things trigger it: the client falls more than `BROADCAST_CAPACITY`
//! updates behind, or a single send takes longer than `SEND_TIMEOUT`. The
//! client reconnects and starts again from a fresh snapshot, as the feed
//! protocol prescribes.
//!
//! Liveness: the server pings every client each ping interval and drops one
//! that has sent nothing, not even a pong, for `MISSED_PINGS` intervals.
//! Silence is checked only when a ping is due, so a vanished peer is dropped
//! between `MISSED_PINGS` and `MISSED_PINGS + 1` intervals after the last frame
//! it sent, which waiting for writes to back up does not bound. Browsers
//! answer pings on their own.
//!
//! Limits: at most `max_clients` feed connections at once; one more is
//! refused with 503 before the upgrade. The feed takes no input, so inbound
//! frames are capped at `MAX_INBOUND_BYTES` and a client that sends a data
//! message is dropped. Refusals are logged at most once a minute, with a
//! count. Per-address limits are not enforced here.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tokio::sync::Semaphore;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::publisher::{Frame, Handle};

/// Longest a single send to a client may take before the client is dropped.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(1);

/// Default time between pings to each client. With `MISSED_PINGS` this drops
/// a vanished client 15 to 20 s after the last frame it sent.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// Ping intervals of silence after which a client is dropped.
const MISSED_PINGS: u32 = 3;

/// Default cap on concurrent feed clients.
pub const MAX_CLIENTS: usize = 500;

/// Largest value `run` accepts for the client cap. Far above any single
/// process's real capacity, and well below the semaphore's own limit, which
/// would otherwise panic after the service's threads have started.
pub const MAX_CLIENTS_LIMIT: usize = 100_000;

/// Shortest time between two "client limit reached" log lines. A client that
/// retries in a loop would otherwise write one line per attempt and make the
/// journal's rate limiter drop lines that matter.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Largest inbound message or frame accepted. Clients only answer pings, and
/// those controls are tiny; the library default is 64 MiB, which a public
/// feed should not buffer for data it never reads.
const MAX_INBOUND_BYTES: usize = 4 * 1024;

#[derive(Clone)]
struct AppState {
    publisher: Handle,
    shutdown: CancellationToken,
    ping_interval: Duration,
    clients: Arc<Semaphore>,
    refusals: Arc<Mutex<Refusals>>,
}

/// Refused connections not yet reported in the log.
#[derive(Default)]
struct Refusals {
    last_logged: Option<Instant>,
    unreported: u64,
}

impl Refusals {
    /// Counts one refusal at `now`; returns how many refusals to report when
    /// a log line is due: the first one, then at most one per `every`.
    fn record(&mut self, now: Instant, every: Duration) -> Option<u64> {
        self.unreported += 1;
        match self.last_logged {
            Some(last) if now.duration_since(last) < every => None,
            _ => {
                self.last_logged = Some(now);
                Some(std::mem::take(&mut self.unreported))
            }
        }
    }
}

/// Builds the router: the feed at `/ws`, serving at most `max_clients` at once.
pub fn router(
    publisher: Handle,
    shutdown: CancellationToken,
    ping_interval: Duration,
    max_clients: usize,
) -> Router {
    Router::new()
        .route("/ws", get(upgrade))
        .with_state(AppState {
            publisher,
            shutdown,
            ping_interval,
            clients: Arc::new(Semaphore::new(max_clients)),
            refusals: Arc::default(),
        })
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    let Ok(permit) = Arc::clone(&state.clients).try_acquire_owned() else {
        let due = state
            .refusals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(Instant::now(), REFUSAL_LOG_INTERVAL);
        if let Some(refused) = due {
            warn!(refused, "client limit reached, refusing connections");
        }
        return (StatusCode::SERVICE_UNAVAILABLE, "too many feed clients").into_response();
    };
    ws.max_message_size(MAX_INBOUND_BYTES)
        .max_frame_size(MAX_INBOUND_BYTES)
        .on_upgrade(move |socket| async move {
            // Held until the client's task ends, however it ends.
            let _permit = permit;
            serve_client(socket, state).await;
        })
}

async fn serve_client(mut socket: WebSocket, state: AppState) {
    let Some(subscription) = state.publisher.subscribe().await else {
        return;
    };
    let mut updates = subscription.updates;
    if !send(&mut socket, Message::Text(subscription.snapshot)).await {
        return;
    }

    let interval = state.ping_interval;
    let silence = interval * MISSED_PINGS;
    let mut pings = interval_at(Instant::now() + interval, interval);
    pings.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();
    loop {
        // Buffered updates come first: when the publisher ends it cancels
        // `shutdown` at the same moment, and its final update must still be
        // forwarded before the client is closed.
        tokio::select! {
            biased;
            update = updates.recv() => match on_update(update) {
                Next::Send(frame) => {
                    if !send(&mut socket, Message::Text(frame)).await {
                        break;
                    }
                }
                Next::Disconnect | Next::Stop => break,
            },
            () = state.shutdown.cancelled() => break,
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    warn!("client sent data, disconnecting");
                    break;
                }
                // Pings are answered by the library; any other frame only
                // shows the peer is alive.
                Some(Ok(_)) => last_heard = Instant::now(),
            },
            _ = pings.tick() => {
                if last_heard.elapsed() >= silence {
                    warn!(?silence, "client silent, disconnecting");
                    break;
                }
                if !send(&mut socket, Message::Ping(Bytes::new())).await {
                    break;
                }
            }
        }
    }
    // Best effort: the peer may already be gone or stuck.
    let _ = timeout(SEND_TIMEOUT, socket.send(Message::Close(None))).await;
    debug!("client disconnected");
}

/// What to do with the result of receiving from the update broadcast.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Forward the frame to the client.
    Send(Frame),
    /// The client fell behind the broadcast buffer: drop it as a slow consumer.
    Disconnect,
    /// The feed ended: the publisher is gone.
    Stop,
}

fn on_update(update: Result<Frame, RecvError>) -> Next {
    match update {
        Ok(frame) => Next::Send(frame),
        Err(RecvError::Lagged(missed)) => {
            warn!(missed, "client fell behind, disconnecting");
            Next::Disconnect
        }
        Err(RecvError::Closed) => Next::Stop,
    }
}

/// Sends one message; `false` when the client must be dropped, after logging why.
async fn send(socket: &mut WebSocket, message: Message) -> bool {
    match timeout(SEND_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => true,
        Ok(Err(err)) => {
            debug!(%err, "send failed, disconnecting");
            false
        }
        Err(_) => {
            warn!("client too slow to receive, disconnecting");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_logged_once_per_interval_with_a_count() {
        let every = Duration::from_secs(60);
        let start = Instant::now();
        let mut refusals = Refusals::default();
        assert_eq!(refusals.record(start, every), Some(1));
        assert_eq!(refusals.record(start + Duration::from_secs(1), every), None);
        assert_eq!(
            refusals.record(start + Duration::from_secs(59), every),
            None
        );
        assert_eq!(refusals.record(start + every, every), Some(3));
        assert_eq!(refusals.record(start + every * 3, every), Some(1));
    }

    #[test]
    fn broadcast_results_map_to_client_actions() {
        let frame = Frame::from("{}");
        assert_eq!(on_update(Ok(frame.clone())), Next::Send(frame));
        assert_eq!(on_update(Err(RecvError::Lagged(3))), Next::Disconnect);
        assert_eq!(on_update(Err(RecvError::Closed)), Next::Stop);
    }
}
