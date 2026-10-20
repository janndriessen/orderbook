//! Random-walk order flow driving the real matcher on its own OS thread.
//!
//! The walk produces commands, the matcher turns them into events, and the
//! thread runs outside Tokio, owns all of its state and talks to the rest of
//! the service only through a bounded queue.

mod book;
mod walk;

use std::thread::{self, JoinHandle};

use protocol::{Envelope, Event, Ts};
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::matcher::SequencedMatcher;

pub use walk::RandomWalk;

/// Capacity of the queue between the source and the publisher.
///
/// The source waits when the queue is full instead of dropping events, so the
/// capacity only has to absorb bursts and publisher scheduling hiccups, not
/// sustained overload. At about 38 events per second, 4096 slots hold well
/// over a minute of output, far more than the publisher's 100 ms tick needs.
pub const QUEUE_CAPACITY: usize = 4096;

/// Starts the source thread.
///
/// The thread runs the walk's commands through the matcher and paces itself
/// to the walk's venue clock. It stops when the receiving end of the queue is
/// dropped.
pub fn spawn(
    mut walk: RandomWalk,
    queue: mpsc::Sender<Envelope<Event>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("source".into()).spawn(move || {
        info!("source started");
        let mut matcher = SequencedMatcher::new();
        'run: loop {
            let step = walk.step();
            for envelope in matcher.apply(step.ts, step.command) {
                // A full queue blocks here: backpressure, never a dropped event.
                if queue.blocking_send(envelope).is_err() {
                    break 'run;
                }
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
