//! Random-walk order flow on its own OS thread.
//!
//! The walk produces commands with venue timestamps; the thread paces itself
//! to the walk's clock, runs outside Tokio, passes every command through the
//! gateway and hands the ones that pass to the engine through a bounded queue.

mod book;
mod walk;

use std::thread::{self, JoinHandle};

use protocol::{Command, Envelope, Ts};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::gateway::Gateway;

pub use walk::RandomWalk;

/// Capacity of the queue between the source and the engine.
///
/// The source waits when the queue is full instead of dropping commands, so
/// the capacity only has to absorb hiccups downstream. It also backs up when
/// the engine waits, which happens only once the publisher has fallen a full
/// event queue behind. The walk
/// spaces commands 20 to 100 ms apart, so at most 50 and on average about 17
/// per second; 1024 slots hold at least 20 seconds of commands.
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Starts the source thread.
///
/// The thread submits the walk's commands, with their venue times, to its own
/// [`Gateway`] and sends those that pass to `queue` as numbered envelopes. A
/// rejected command is logged and dropped; it never enters the event stream. The thread paces itself
/// to the walk's venue clock and stops when the receiving end of the queue is
/// dropped.
pub fn spawn(
    mut walk: RandomWalk,
    queue: mpsc::Sender<Envelope<Command>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("source".into()).spawn(move || {
        info!("source started");
        let mut gateway = Gateway::new();
        loop {
            let step = walk.step();
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
