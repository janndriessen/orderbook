//! WebSocket endpoint serving the market-data feed.
//!
//! Slow-consumer policy: a client that cannot keep up is disconnected, never
//! buffered without bound and never allowed to hold back the others. Two
//! things trigger it: the client falls more than `BROADCAST_CAPACITY`
//! updates behind, or a single send takes longer than `SEND_TIMEOUT`. The
//! client reconnects and starts again from a fresh snapshot, as the feed
//! protocol prescribes.

use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::publisher::{self, Frame, Handle};

/// Longest a single send to a client may take before the client is dropped.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone)]
struct AppState {
    publisher: Handle,
    shutdown: CancellationToken,
}

/// Builds the router: the feed at `/ws`.
pub fn router(publisher: Handle, shutdown: CancellationToken) -> Router {
    Router::new()
        .route("/ws", get(upgrade))
        .with_state(AppState {
            publisher,
            shutdown,
        })
}

async fn upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| serve_client(socket, state))
}

async fn serve_client(mut socket: WebSocket, state: AppState) {
    let Some(subscription) = state.publisher.subscribe().await else {
        return;
    };
    let mut updates = subscription.updates;
    if !send(&mut socket, publisher::encode(&subscription.snapshot)).await {
        return;
    }

    loop {
        tokio::select! {
            () = state.shutdown.cancelled() => break,
            update = updates.recv() => match on_update(update) {
                Next::Send(frame) => {
                    if !send(&mut socket, frame).await {
                        break;
                    }
                }
                Next::Disconnect | Next::Stop => break,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                // Pings are answered by the library; the feed takes no input.
                Some(Ok(_)) => {}
            },
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

/// Sends one frame; `false` when the client must be dropped, after logging why.
async fn send(socket: &mut WebSocket, frame: Frame) -> bool {
    match timeout(SEND_TIMEOUT, socket.send(Message::Text(frame))).await {
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
    fn broadcast_results_map_to_client_actions() {
        let frame = Frame::from("{}");
        assert_eq!(on_update(Ok(frame.clone())), Next::Send(frame));
        assert_eq!(on_update(Err(RecvError::Lagged(3))), Next::Disconnect);
        assert_eq!(on_update(Err(RecvError::Closed)), Next::Stop);
    }
}
