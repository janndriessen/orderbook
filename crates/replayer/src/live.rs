//! Serving the live Hyperliquid book: the connections feed an [`L2Flow`], its
//! commands go to the engine as they come, and a lost connection is replaced.
//!
//! Nothing is paced: a command goes to the engine the moment the push it
//! belongs to is complete. While the connections are down the market is
//! halted, so that clients can tell an outage from a quiet market. Timestamps
//! come from the source and never from a clock here: the halt is stamped with
//! the time of the last command sent, and none is sent before anything was
//! replayed, as there is nothing to halt. The reopening is stamped with the
//! time of the first command of the new connections, or with the time of
//! their first push when the book did not change and it makes none.
//!
//! When any of the seven connections fails (closed, lost, silent for too
//! long, a message that does not apply, or no push has merged for too long
//! although lines keep coming), all of them are dropped and opened again after
//! a pause that doubles up to a limit. What was learned from the old
//! connections is forgotten, but the flow keeps its translator, whose book
//! mirrors what the engine holds, so the first push of the new connections
//! replaces the whole book and the engine catches up with what it missed.

use std::io::{self, Write};
use std::iter;
use std::time::Duration;

use engine::{CancellationToken, Step};
use hyperliquid_recorder::{CONNECTIONS, Connections};
use protocol::{Command, TradingStatus, Ts};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout, timeout_at};
use tracing::{info, warn};

use crate::L2Flow;

/// Commands waiting for the engine. When it is full the connections are not
/// read until the engine has caught up, which holds the source back instead of
/// dropping commands.
const QUEUE_CAPACITY: usize = 4096;

/// How long opening the seven connections may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How the connections are opened again after a failure, and when a
/// connection that stays up counts as failed.
///
/// The pause before connecting again starts at `initial`, doubles after each
/// failure up to `max`, and starts over once a connection has produced
/// commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// The first pause.
    pub initial: Duration,
    /// The longest pause.
    pub max: Duration,
    /// How long the connections may stay up without a push merging, counted
    /// from when they are open and from each merged push. Books skipped for
    /// being behind what was already replayed count as well while their
    /// grouping's time moves forward: the source is catching up, whereas a
    /// frozen clock repeating the same time is not. The source's own output
    /// is what a consumer must watch: with every connection sending but the
    /// groupings' times never matching, no push completes, the served book
    /// freezes and the trades held for the next push pile up.
    pub progress: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            // About twenty pushes at the source's cadence of half a second.
            progress: Duration::from_secs(10),
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

/// What outlives one set of connections: how many lines all of them have
/// delivered, and what the engine was last told, which decides whether a
/// status command is needed and what time it carries.
#[derive(Default)]
struct Sent {
    /// Lines of all connections so far.
    number: usize,
    /// The time of the latest command sent, 0 while none was; later commands
    /// never go back.
    ts: Ts,
    halted: bool,
}

impl Sent {
    /// Sends `status` at `ts` unless the engine already has it, or unless
    /// it is a halt before any command was sent: the book is empty, there is
    /// nothing to halt, and the halt would carry no time from the source.
    /// Returns false when the engine no longer takes commands.
    async fn status(&mut self, steps: &mpsc::Sender<Step>, status: TradingStatus, ts: Ts) -> bool {
        let halt = status == TradingStatus::Halted;
        if self.halted == halt || (halt && self.ts == 0) {
            return true;
        }
        self.halted = status == TradingStatus::Halted;
        self.ts = self.ts.max(ts);
        let step = Step {
            command: Command::SetTradingStatus { status },
            ts: self.ts,
            wait: Duration::ZERO,
        };
        steps.send(step).await.is_ok()
    }
}

/// Lines waiting for the recording's writer thread.
const RECORD_QUEUE: usize = 1024;

/// Writes the lines of the recording on a thread of its own, so that a slow
/// disk holds up a bounded queue and not the connections task's runtime.
struct Recorder {
    lines: Option<mpsc::Sender<String>>,
    thread: Option<std::thread::JoinHandle<io::Result<()>>>,
}

impl Recorder {
    fn start(mut out: Box<dyn Write + Send>) -> io::Result<Self> {
        let (lines, mut queue) = mpsc::channel::<String>(RECORD_QUEUE);
        let thread = std::thread::Builder::new()
            .name("record".into())
            .spawn(move || {
                while let Some(line) = queue.blocking_recv() {
                    writeln!(out, "{line}")?;
                    out.flush()?;
                }
                Ok(())
            })?;
        Ok(Self {
            lines: Some(lines),
            thread: Some(thread),
        })
    }

    /// Queues a line. If the writer has stopped, returns what stopped it.
    async fn write(&mut self, line: String) -> io::Result<()> {
        let sent = match &self.lines {
            Some(lines) => lines.send(line).await.is_ok(),
            None => false,
        };
        if sent {
            return Ok(());
        }
        let ended = self.join().await;
        Err(ended
            .err()
            .unwrap_or_else(|| io::Error::other("the recording writer stopped")))
    }

    /// Waits for the queued lines to be written.
    async fn finish(mut self) -> io::Result<()> {
        self.join().await
    }

    async fn join(&mut self) -> io::Result<()> {
        self.lines = None;
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match tokio::task::spawn_blocking(move || thread.join()).await {
            Ok(Ok(result)) => result,
            _ => Err(io::Error::other("the recording writer panicked")),
        }
    }
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
/// the connections task panicked. The end of the connections task for any
/// reason stops the service.
pub async fn serve(
    listener: TcpListener,
    url: &str,
    record: Option<Box<dyn Write + Send>>,
    shutdown: CancellationToken,
    retry: Retry,
    max_clients: usize,
) -> io::Result<()> {
    let (steps, mut queue) = mpsc::channel(QUEUE_CAPACITY);
    let recorder = record.map(Recorder::start).transpose()?;
    // However the task ends (a return or a panic), the guard stops the rest:
    // without the connections the engine's book would only freeze.
    let feeder = tokio::spawn({
        let url = url.to_owned();
        let shutdown = shutdown.clone();
        async move {
            let _stop = shutdown.clone().drop_guard();
            feed(url, steps, shutdown, recorder, retry).await
        }
    });
    // The engine's source thread blocks here between commands. It ends when
    // the connections task drops the sender, which that task does on shutdown
    // and whenever it stops for any other reason.
    let flow = iter::from_fn(move || queue.blocking_recv());
    let served = engine::run(
        listener,
        flow,
        shutdown.clone(),
        engine::PING_INTERVAL,
        max_clients,
    )
    .await;
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
    mut record: Option<Recorder>,
    retry: Retry,
) -> io::Result<()> {
    let run = feed_loop(&url, &steps, &shutdown, &mut record, retry).await;
    // The last lines are on disk before the service reports it has stopped.
    let written = match record {
        Some(recorder) => recorder.finish().await,
        None => Ok(()),
    };
    run.and(written)
}

/// The loop of [`feed`].
async fn feed_loop(
    url: &str,
    steps: &mpsc::Sender<Step>,
    shutdown: &CancellationToken,
    record: &mut Option<Recorder>,
    retry: Retry,
) -> io::Result<()> {
    let mut flow = L2Flow::new(engine::market(), CONNECTIONS);
    let mut backoff = retry.initial;
    let mut sent = Sent::default();
    loop {
        let run = connected(
            url,
            &mut flow,
            steps,
            record,
            &mut sent,
            &mut backoff,
            retry,
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
        // Stopping returns above and sends nothing.
        if !sent.status(steps, TradingStatus::Halted, sent.ts).await {
            return Ok(());
        }
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            () = sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(retry.max);
    }
}

/// Opens the connections and feeds their lines through `flow` until one
/// fails. The market is reopened, if it is halted, when the first push of the
/// new connections merges, before that push's commands.
///
/// `sent.number` counts the lines of all connections so far, and `backoff` is
/// put back to `retry.initial` when lines make commands. A connection that
/// merges no push and skips no stale book with a later time than before for
/// `retry.progress` fails, the first push included: opening is done by then,
/// and the first push needs only one book per grouping.
async fn connected(
    url: &str,
    flow: &mut L2Flow,
    steps: &mpsc::Sender<Step>,
    record: &mut Option<Recorder>,
    sent: &mut Sent,
    backoff: &mut Duration,
    retry: Retry,
) -> io::Result<Outcome> {
    let mut connections = match timeout(CONNECT_TIMEOUT, Connections::connect(url)).await {
        Ok(Ok(connections)) => connections,
        Ok(Err(err)) => return Ok(Outcome::Failed(err.to_string())),
        Err(_) => return Ok(Outcome::Failed("connecting took too long".to_owned())),
    };
    info!("connected to the source");
    let mut progressed = flow.progress();
    let mut deadline = Instant::now() + retry.progress;
    loop {
        let value = match timeout_at(deadline, connections.next()).await {
            Ok(Ok(value)) => value,
            Ok(Err(err)) => return Ok(Outcome::Failed(err.to_string())),
            Err(_) => {
                return Ok(Outcome::Failed(format!(
                    "no push merged for {:?} although the connections are open",
                    retry.progress
                )));
            }
        };
        let text = value.to_string();
        if let Some(recorder) = record.as_mut() {
            recorder.write(text.clone()).await?;
        }
        sent.number += 1;
        let commands = match flow.line(sent.number, &text) {
            Ok(commands) => commands,
            Err(err) => return Ok(Outcome::Failed(err.to_string())),
        };
        let moved = flow.progress() > progressed;
        progressed = flow.progress();
        if !commands.is_empty() {
            *backoff = retry.initial;
        }
        // The first push of the new connections reopens the market even when
        // it leaves the book unchanged and so makes no commands. Its commands
        // are stamped no later than the push, so the opening takes the first
        // one's time to keep the times from going back.
        if let Some(merged) = flow.last_merged() {
            let ts = commands.first().map_or(merged, |&(ts, _)| ts);
            if !sent.status(steps, TradingStatus::Open, ts).await {
                return Ok(Outcome::EngineGone);
            }
        }
        for (ts, command) in commands {
            sent.ts = sent.ts.max(ts);
            let step = Step {
                command,
                ts,
                wait: Duration::ZERO,
            };
            if steps.send(step).await.is_err() {
                return Ok(Outcome::EngineGone);
            }
        }
        // Reset only once the engine has the commands: waiting for it to
        // catch up is not the source's silence.
        if moved {
            deadline = Instant::now() + retry.progress;
        }
    }
}
