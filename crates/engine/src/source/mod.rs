//! Order flow on its own OS thread.
//!
//! Any iterator of [`Step`]s can drive the source; [`RandomWalk`] is the
//! built-in one. The thread paces itself to the flow's clock, runs outside
//! Tokio, passes every command through the gateway and hands the ones that
//! pass to the engine through a bounded queue.

mod book;
mod walk;

use std::thread::{self, JoinHandle};
use std::time::Duration;

use protocol::{Command, Envelope, Ts};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::gateway::Gateway;

pub use walk::RandomWalk;

/// One step of an order flow: a command, its venue time and the pause before
/// the next step.
#[derive(Debug)]
pub struct Step {
    /// The request to send to the matcher.
    pub command: Command,
    /// Venue time of the command.
    pub ts: Ts,
    /// How long a real-time consumer waits before asking for the next step.
    pub wait: Duration,
}

/// Capacity of the queue between the source and the engine.
///
/// The source waits when the queue is full instead of dropping commands, so
/// the capacity only has to absorb hiccups downstream. It also backs up when
/// the engine waits, which happens only once the publisher has fallen a full
/// event queue behind. The size fits the random walk, which spaces commands
/// 20 to 100 ms apart, so at most 50 and on average about 17 per second; 1024
/// slots hold at least 20 seconds of its commands.
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Starts the source thread.
///
/// The thread submits the flow's commands, with their venue times, to its own
/// [`Gateway`] and sends those that pass to `queue` as numbered envelopes. A
/// rejected command is logged and dropped; it never enters the event stream.
/// The thread paces itself to the flow's venue clock. It stops when the flow
/// ends, or on the first send of an accepted command after the receiving end
/// of the queue was dropped. Stopping drops the sender, which closes the queue
/// and lets the engine finish on its own.
pub fn spawn(
    flow: impl Iterator<Item = Step> + Send + 'static,
    queue: mpsc::Sender<Envelope<Command>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("source".into()).spawn(move || {
        info!("source started");
        let mut gateway = Gateway::new();
        for step in flow {
            match gateway.submit(step.ts, step.command) {
                // A full queue blocks here: backpressure, never a dropped command.
                Ok(envelope) => {
                    if queue.blocking_send(envelope).is_err() {
                        break;
                    }
                }
                Err(reject) => warn!(%reject, command = ?step.command, "gateway rejected command"),
            }
            thread::sleep(step.wait);
        }
        debug!("source stopped");
    })
}

/// Current wall-clock time as venue time, used only to start a walk.
pub fn now_ts() -> Ts {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use protocol::{OrderType, Side, TimeInForce};
    use tokio::time::timeout;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    fn place(order_id: u64, ts: Ts) -> Step {
        Step {
            command: Command::Place {
                order_id,
                side: Side::Buy,
                order_type: OrderType::Limit {
                    price: 100,
                    time_in_force: TimeInForce::Gtc,
                },
                qty: 1,
            },
            ts,
            wait: Duration::ZERO,
        }
    }

    async fn next(queue: &mut mpsc::Receiver<Envelope<Command>>) -> Option<Envelope<Command>> {
        timeout(WAIT, queue.recv()).await.expect("queue answers")
    }

    #[tokio::test]
    async fn a_rejected_step_leaves_no_gap_and_the_end_of_the_flow_closes_the_queue() {
        let flow = vec![place(1, 10), place(1, 20), place(2, 30)];
        let (tx, mut queue) = mpsc::channel(2);
        let thread = spawn(flow.into_iter(), tx).expect("thread starts");

        let first = next(&mut queue).await.expect("first command passes");
        let second = next(&mut queue).await.expect("second command passes");
        assert_eq!((first.seq, first.ts), (1, 10));
        assert!(matches!(first.body, Command::Place { order_id: 1, .. }));
        assert_eq!((second.seq, second.ts), (2, 30));
        assert!(matches!(second.body, Command::Place { order_id: 2, .. }));
        assert!(next(&mut queue).await.is_none());
        // The queue closes only as the thread returns, so this cannot block.
        thread.join().expect("source thread does not panic");
    }
}
