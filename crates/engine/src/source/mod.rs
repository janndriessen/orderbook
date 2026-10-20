//! Random-walk order flow on its own OS thread.
//!
//! The walk produces commands with venue timestamps; the thread paces itself
//! to the walk's clock, runs outside Tokio, and hands the commands to the
//! engine through a bounded queue.

mod book;
mod walk;

use std::thread::{self, JoinHandle};

use protocol::{Command, Ts};
use tokio::sync::mpsc;
use tracing::{debug, info};

pub use walk::RandomWalk;

/// Capacity of the queue between the source and the engine.
///
/// The source waits when the queue is full instead of dropping commands, so
/// the capacity only has to absorb engine scheduling hiccups. The walk
/// spaces commands 20 to 100 ms apart, so at most 50 and on average about 17
/// per second; 1024 slots hold at least 20 seconds of commands.
pub const COMMAND_QUEUE_CAPACITY: usize = 1024;

/// Starts the source thread.
///
/// The thread sends the walk's commands, with their venue times, to `queue`
/// and paces itself to the walk's venue clock. It stops when the receiving
/// end of the queue is dropped.
pub fn spawn(
    mut walk: RandomWalk,
    queue: mpsc::Sender<(Ts, Command)>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("source".into()).spawn(move || {
        info!("source started");
        loop {
            let step = walk.step();
            // A full queue blocks here: backpressure, never a dropped command.
            if queue.blocking_send((step.ts, step.command)).is_err() {
                break;
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
