//! Turns the engine's event stream into the sequenced market-data feed.
//!
//! A single Tokio task owns the [`Publisher`]. It collects events as they
//! arrive, publishes one combined update every [`TICK`] and answers
//! subscription requests between ticks.

mod book;
mod state;

use std::time::Duration;

use protocol::{Envelope, Event, MarketInfo};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub use self::state::{Frame, Publisher, Subscription, encode};

/// Time between two updates.
pub const TICK: Duration = Duration::from_millis(100);

/// Cloneable access to the running publisher, used by the WebSocket layer.
#[derive(Debug, Clone)]
pub struct Handle {
    requests: mpsc::Sender<oneshot::Sender<Subscription>>,
}

impl Handle {
    /// Asks for a snapshot plus a subscription to the updates that follow it.
    ///
    /// Returns `None` once the publisher has stopped.
    pub async fn subscribe(&self) -> Option<Subscription> {
        let (reply, response) = oneshot::channel();
        self.requests.send(reply).await.ok()?;
        response.await.ok()
    }
}

/// Starts the publisher task.
///
/// It runs until `shutdown` fires or the event queue closes. Ending for any
/// reason cancels `shutdown`, and the update broadcast closes with the task,
/// which disconnects every client.
pub fn spawn(
    market: MarketInfo,
    mut events: mpsc::Receiver<Envelope<Event>>,
    shutdown: CancellationToken,
) -> (Handle, JoinHandle<()>) {
    let (requests, mut subscriptions) = mpsc::channel::<oneshot::Sender<Subscription>>(64);
    let task = tokio::spawn(async move {
        // However this task ends, the rest of the service stops with it
        // instead of serving clients that can no longer get updates.
        let _stop = shutdown.clone().drop_guard();
        let mut publisher = Publisher::new(market);
        let mut ticker = time::interval(TICK);
        // After a stall, resume at the normal pace instead of firing a burst
        // of catch-up ticks, each of which would be a separate message.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                event = events.recv() => match event {
                    Some(event) => publisher.ingest(event),
                    None => {
                        warn!("event source closed, stopping publisher");
                        break;
                    }
                },
                _ = ticker.tick() => {
                    publisher.tick();
                }
                Some(reply) = subscriptions.recv() => {
                    // The client may have gone away while waiting; nothing to do.
                    let _ = reply.send(publisher.subscribe());
                }
            }
        }
        debug!("publisher stopped");
    });
    (Handle { requests }, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::market;

    #[tokio::test]
    async fn closed_event_queue_stops_the_publisher_and_cancels_shutdown() {
        let (events, queue) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        let (_handle, task) = spawn(market(), queue, shutdown.clone());

        drop(events);

        let wait = Duration::from_secs(5);
        time::timeout(wait, task)
            .await
            .expect("publisher stops")
            .expect("publisher does not panic");
        assert!(shutdown.is_cancelled());
    }
}
