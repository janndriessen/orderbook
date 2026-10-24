//! Turns the engine's event stream into the sequenced market-data feed.
//!
//! A single Tokio task owns the [`Publisher`]. It collects events as they
//! arrive, publishes one combined update every [`TICK`], or sooner when a
//! burst reaches `MAX_EVENTS_PER_UPDATE`, and answers subscription requests
//! between ticks. Every [`stats::WINDOW`] it logs how
//! many updates, events and bytes it sent.

mod book;
mod state;
mod stats;

use std::time::Duration;

use protocol::{Envelope, Event, MarketInfo};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

pub use self::state::{Frame, Publisher, Subscription};

/// Time between two updates: the batching interval production venues use for
/// level-2 feeds. A tick sends nothing when nothing changed, so the message
/// rate follows the source, not this constant.
pub const TICK: Duration = Duration::from_millis(50);

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
/// It runs until `shutdown` fires or the event queue closes; in the second
/// case the events received since the last tick are published first. Each
/// queue item holds all events of one command and is taken in one step, so an
/// update never contains part of a command's events. Ending for any reason
/// cancels `shutdown`, and the update broadcast closes with the task, which
/// disconnects every client.
pub fn spawn(
    market: MarketInfo,
    mut events: mpsc::Receiver<Vec<Envelope<Event>>>,
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
        let mut summary = time::interval_at(time::Instant::now() + stats::WINDOW, stats::WINDOW);
        summary.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                batch = events.recv() => match batch {
                    Some(batch) => publisher.ingest(batch),
                    None => {
                        publisher.tick();
                        warn!("event source closed, stopping publisher");
                        break;
                    }
                },
                _ = ticker.tick() => {
                    publisher.tick();
                }
                _ = summary.tick() => publisher.take_stats().log(),
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
    use protocol::FeedMessage;
    use tokio::sync::broadcast::error::RecvError;

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

    #[tokio::test]
    async fn events_received_before_the_queue_closed_are_published_first() {
        let (events, queue) = mpsc::channel(4);
        let shutdown = CancellationToken::new();
        let (handle, task) = spawn(market(), queue, shutdown);
        let mut subscription = handle.subscribe().await.expect("publisher is running");
        // Lets the immediate first tick pass, so the next one is a tick away.
        time::sleep(Duration::from_millis(20)).await;

        let body = Event::Canceled {
            order_id: 1,
            leaves_qty: 3,
        };
        events
            .send(vec![Envelope {
                seq: 1,
                ts: 0,
                body,
            }])
            .await
            .expect("queue is open");
        drop(events);

        let frame = subscription.updates.recv().await.expect("final update");
        let FeedMessage::Update { events, .. } = serde_json::from_str(frame.as_str()).unwrap()
        else {
            panic!("expected an update");
        };
        assert_eq!(events.len(), 1);
        task.await.expect("publisher does not panic");
        assert_eq!(
            subscription.updates.recv().await.unwrap_err(),
            RecvError::Closed
        );
    }
}
