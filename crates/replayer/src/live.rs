//! Serving the live Hyperliquid book: the connections feed an [`L2Flow`], its
//! commands go to the engine as they come, and a lost connection is replaced.
//!
//! Nothing is paced: a command goes to the engine the moment the push it
//! belongs to is complete. When any of the seven connections fails (closed,
//! lost, silent for too long, or a message that does not apply), all of them
//! are dropped and opened again after a pause that doubles up to a limit. What
//! was learned from the old connections is forgotten, but the flow keeps its
//! translator, whose book mirrors what the engine holds, so the first push of
//! the new connections replaces the whole book and the engine catches up with
//! what it missed.

use std::io::{self, Write};
use std::iter;
use std::time::Duration;

use engine::{CancellationToken, Step};
use hyperliquid_recorder::{CONNECTIONS, Connections};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};

use crate::L2Flow;

/// Commands waiting for the engine. When it is full the connections are not
/// read until the engine has caught up, which holds the source back instead of
/// dropping commands.
const QUEUE_CAPACITY: usize = 4096;

/// How long opening the seven connections may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The pause before connecting again: it starts at `initial`, doubles after
/// each failure up to `max`, and starts over once a connection has produced
/// commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// The first pause.
    pub initial: Duration,
    /// The longest pause.
    pub max: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

/// How a connection's run ended.
enum Outcome {
    /// It failed, for this reason, and the connections should be opened again.
    Failed(String),
    /// The engine no longer takes commands.
    EngineGone,
}

/// Serves the live book at `listener` until `shutdown` fires or the service
/// fails, with the source at `url`. Every line received is also written to
/// `record`, if given, as one JSON object per line.
///
/// Waits for the connections task, the engine and the server to stop before
/// it returns.
///
/// # Errors
///
/// Returns an error if the service failed, the record cannot be written, or
/// the connections task panicked.
pub async fn serve(
    listener: TcpListener,
    url: &str,
    record: Option<Box<dyn Write + Send>>,
    shutdown: CancellationToken,
    retry: Retry,
) -> io::Result<()> {
    let (steps, mut queue) = mpsc::channel(QUEUE_CAPACITY);
    let feeder = tokio::spawn(feed(url.to_owned(), steps, shutdown.clone(), record, retry));
    // The engine's source thread blocks here between commands. It ends when
    // the connections task drops the sender, which that task does on shutdown
    // and whenever it stops for any other reason.
    let flow = iter::from_fn(move || queue.blocking_recv());
    let served = engine::run(listener, flow, shutdown.clone(), engine::PING_INTERVAL).await;
    shutdown.cancel();
    let fed = feeder.await;
    served?;
    fed.map_err(|err| io::Error::other(format!("the connections task failed: {err}")))?
}

/// Reads the source and sends the commands to `steps`, reconnecting after a
/// failure, until `shutdown` fires or the engine stops taking commands.
async fn feed(
    url: String,
    steps: mpsc::Sender<Step>,
    shutdown: CancellationToken,
    mut record: Option<Box<dyn Write + Send>>,
    retry: Retry,
) -> io::Result<()> {
    let mut flow = L2Flow::new(engine::market(), CONNECTIONS);
    let mut number = 0;
    let mut backoff = retry.initial;
    loop {
        let run = connected(
            &url,
            &mut flow,
            &steps,
            &mut record,
            &mut number,
            &mut backoff,
            retry.initial,
        );
        let outcome = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            outcome = run => outcome?,
        };
        let reason = match outcome {
            Outcome::EngineGone => return Ok(()),
            Outcome::Failed(reason) => reason,
        };
        warn!(
            reason,
            ?backoff,
            "connections to the source ended, opening them again"
        );
        flow.reset_connection();
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(retry.max);
    }
}

/// Opens the connections and feeds their lines through `flow` until one
/// fails. `number` counts the lines of all connections so far, and `backoff`
/// is put back to `initial` when lines make commands.
async fn connected(
    url: &str,
    flow: &mut L2Flow,
    steps: &mpsc::Sender<Step>,
    record: &mut Option<Box<dyn Write + Send>>,
    number: &mut usize,
    backoff: &mut Duration,
    initial: Duration,
) -> io::Result<Outcome> {
    let mut connections = match timeout(CONNECT_TIMEOUT, Connections::connect(url)).await {
        Ok(Ok(connections)) => connections,
        Ok(Err(err)) => return Ok(Outcome::Failed(err.to_string())),
        Err(_) => return Ok(Outcome::Failed("connecting took too long".to_owned())),
    };
    info!("connected to the source");
    loop {
        let value = match connections.next().await {
            Ok(value) => value,
            Err(err) => return Ok(Outcome::Failed(err.to_string())),
        };
        let text = value.to_string();
        if let Some(out) = record.as_mut() {
            writeln!(out, "{text}")?;
            out.flush()?;
        }
        *number += 1;
        let commands = match flow.line(*number, &text) {
            Ok(commands) => commands,
            Err(err) => return Ok(Outcome::Failed(err.to_string())),
        };
        if !commands.is_empty() {
            *backoff = initial;
        }
        for (ts, command) in commands {
            let step = Step {
                command,
                ts,
                wait: Duration::ZERO,
            };
            if steps.send(step).await.is_err() {
                return Ok(Outcome::EngineGone);
            }
        }
    }
}
