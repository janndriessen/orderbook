//! The matcher with the event sequencing around it, and the thread it runs on.

use std::thread::{self, JoinHandle};

use matching::Matcher;
use protocol::{Command, Envelope, Event, Seq, Ts};
use tokio::sync::mpsc;
use tracing::{debug, info};

/// Capacity of the queue between the engine thread and the publisher, in
/// commands.
///
/// A slot holds all events of one command, so the publisher takes them in one
/// step and no update splits them. A taker produces one trade per resting
/// order it fills plus an update per level, so a batch has no small fixed
/// bound: the queue bounds commands, and its memory is the capacity times the
/// events per command. The engine waits when the queue is full instead of
/// dropping events, so the capacity only has to absorb bursts and publisher
/// scheduling hiccups, not sustained overload. The walk's order flow produces
/// about 17 commands per second (measured: about 2.3 events per command), so
/// 4096 slots hold several minutes of output, far more than the publisher's
/// 50 ms tick needs.
pub const EVENT_QUEUE_CAPACITY: usize = 4096;

/// A matcher that numbers its output.
///
/// Event sequence numbers run gap-free from 1 across all commands, and each
/// event carries the venue time of the command that caused it, so the same
/// commands always produce the same envelopes.
#[derive(Debug, Default)]
pub struct SequencedMatcher {
    matcher: Matcher,
    /// Sequence number of the last event produced, 0 before the first.
    last_seq: Seq,
}

impl SequencedMatcher {
    /// A matcher over an empty book, before its first event.
    pub fn new() -> Self {
        Self::default()
    }

    /// Runs one command and returns its events in order, sequenced and
    /// stamped with `ts`, the command's venue time.
    pub fn apply(&mut self, ts: Ts, command: Command) -> Vec<Envelope<Event>> {
        self.matcher
            .apply(command)
            .into_iter()
            .map(|body| {
                self.last_seq += 1;
                Envelope {
                    seq: self.last_seq,
                    ts,
                    body,
                }
            })
            .collect()
    }
}

/// Starts the engine thread.
///
/// The thread owns a [`SequencedMatcher`], runs every command from `commands`
/// through it in arrival order, stamped with the envelope's venue time, and
/// sends the resulting events to `events`, one item per command. It stops once
/// `commands` is closed and drained, or as soon as a send fails because the
/// receiving end of `events` is gone; while it waits for a command it cannot
/// notice that.
///
/// It also stops, with a panic and before matching it, at a command whose
/// `seq` is not the one after the previous command's, starting from 1:
/// matching input other than the sequence the gateway numbered could leave a
/// book that no replay reproduces. The events of the commands matched before
/// are still delivered.
///
/// In every case the command receiver is dropped with the thread, which tells
/// the sender to stop.
pub fn spawn(
    mut commands: mpsc::Receiver<Envelope<Command>>,
    events: mpsc::Sender<Vec<Envelope<Event>>>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name("engine".into()).spawn(move || {
        info!("engine started");
        let mut matcher = SequencedMatcher::new();
        let mut next_seq: Seq = 1;
        'run: while let Some(command) = commands.blocking_recv() {
            assert_eq!(
                command.seq, next_seq,
                "the command sequence has a gap or is out of order"
            );
            next_seq += 1;
            let batch = matcher.apply(command.ts, command.body);
            // A full queue blocks here: backpressure, never a dropped event.
            if events.blocking_send(batch).is_err() {
                break 'run;
            }
        }
        debug!("engine stopped");
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use protocol::{OrderType, Side, TimeInForce};
    use tokio::time::{sleep, timeout};

    use super::*;

    fn limit(order_id: u64, side: Side, price: i64, qty: u64, tif: TimeInForce) -> Command {
        Command::Place {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force: tif,
            },
            qty,
        }
    }

    /// Rests a bid, takes it with a larger sell (accepted, trade, canceled
    /// remainder and level updates), then cancels an order that is gone.
    fn commands() -> [(Ts, Command); 3] {
        [
            (1_000, limit(1, Side::Buy, 100, 5, TimeInForce::Gtc)),
            (2_500, limit(2, Side::Sell, 100, 8, TimeInForce::Ioc)),
            (2_500, Command::Cancel { order_id: 1 }),
        ]
    }

    /// `commands()` as the gateway would hand them over, numbered from 1.
    fn envelopes() -> Vec<Envelope<Command>> {
        (1..)
            .zip(commands())
            .map(|(seq, (ts, body))| Envelope { seq, ts, body })
            .collect()
    }

    #[test]
    fn events_are_gap_free_stamped_and_match_the_matcher() {
        let mut sequenced = SequencedMatcher::new();
        let mut reference = Matcher::new();
        let mut next_seq = 1;
        let mut counts = Vec::new();
        let mut last = Vec::new();
        for (ts, command) in commands() {
            let expected = reference.apply(command);
            last = sequenced.apply(ts, command);
            counts.push(last.len());
            assert_eq!(
                last.iter().map(|e| &e.body).collect::<Vec<_>>(),
                expected.iter().collect::<Vec<_>>()
            );
            for envelope in &last {
                assert_eq!(envelope.seq, next_seq);
                assert_eq!(envelope.ts, ts);
                next_seq += 1;
            }
        }
        assert!(counts[1] > 1, "a command can produce several events");
        assert!(
            matches!(
                last[..],
                [Envelope {
                    body: Event::CancelRejected { .. },
                    ..
                }]
            ),
            "the last command is refused and still takes a sequence number"
        );
    }

    const WAIT: Duration = Duration::from_secs(5);

    /// What the matcher alone returns for `commands()`, sequenced from 1, one
    /// batch per command.
    fn expected() -> Vec<Vec<Envelope<Event>>> {
        let mut sequenced = SequencedMatcher::new();
        commands()
            .into_iter()
            .map(|(ts, command)| sequenced.apply(ts, command))
            .collect()
    }

    /// Waits for the thread to end, failing instead of hanging, and returns
    /// whether it panicked.
    async fn finish(handle: JoinHandle<()>) -> thread::Result<()> {
        timeout(WAIT, async {
            while !handle.is_finished() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("engine thread stops");
        handle.join()
    }

    /// Waits for the thread to end normally.
    async fn join(handle: JoinHandle<()>) {
        finish(handle).await.expect("engine thread does not panic");
    }

    async fn next(events: &mut mpsc::Receiver<Vec<Envelope<Event>>>) -> Vec<Envelope<Event>> {
        timeout(WAIT, events.recv())
            .await
            .expect("an event arrives")
            .expect("event queue stays open")
    }

    #[tokio::test]
    async fn thread_emits_the_sequenced_events_of_its_commands() {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut events) = mpsc::channel(64);
        let thread = spawn(command_rx, event_tx).expect("thread starts");
        for command in envelopes() {
            command_tx.send(command).await.expect("engine is running");
        }

        let expected = expected();
        for want in &expected {
            assert_eq!(&next(&mut events).await, want);
        }
        drop(command_tx);
        join(thread).await;
        assert!(events.recv().await.is_none(), "no further events");
    }

    #[tokio::test]
    async fn closing_the_command_queue_delivers_what_is_queued_then_stops() {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut events) = mpsc::channel(64);
        for command in envelopes() {
            command_tx.send(command).await.expect("queue has room");
        }
        drop(command_tx);
        let thread = spawn(command_rx, event_tx).expect("thread starts");

        join(thread).await;
        let mut delivered = Vec::new();
        while let Some(batch) = events.recv().await {
            delivered.push(batch);
        }
        assert_eq!(delivered, expected());
    }

    #[tokio::test]
    async fn dropping_the_event_receiver_stops_the_thread_and_closes_the_command_queue() {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, events) = mpsc::channel(64);
        let thread = spawn(command_rx, event_tx).expect("thread starts");

        drop(events);
        let first = envelopes().remove(0);
        command_tx.send(first).await.expect("engine is running");

        join(thread).await;
        assert!(command_tx.is_closed());
    }

    #[tokio::test]
    async fn a_full_event_queue_delays_events_without_dropping_any() {
        let (command_tx, command_rx) = mpsc::channel(8);
        let (event_tx, mut events) = mpsc::channel(1);
        let thread = spawn(command_rx, event_tx).expect("thread starts");
        for command in envelopes() {
            command_tx.send(command).await.expect("engine is running");
        }
        // Nobody reads until the first batch fills the one-slot queue, so the
        // engine's next send has to wait for the reads below.
        timeout(WAIT, async {
            while events.len() < events.max_capacity() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the engine fills the event queue");

        let expected = expected();
        assert!(expected.len() > 1, "more batches than the queue holds");
        for want in &expected {
            assert_eq!(&next(&mut events).await, want);
        }
        drop(command_tx);
        join(thread).await;
    }

    #[tokio::test]
    async fn an_out_of_sequence_command_stops_the_thread_with_a_panic() {
        // Each case: the seqs sent, and how many commands are matched before
        // the one out of sequence.
        for (seqs, matched) in [(vec![1, 3], 1), (vec![1, 1], 1), (vec![2], 0)] {
            let (command_tx, command_rx) = mpsc::channel(8);
            let (event_tx, mut events) = mpsc::channel(64);
            let thread = spawn(command_rx, event_tx).expect("thread starts");
            for (&seq, (ts, body)) in seqs.iter().zip(commands()) {
                let envelope = Envelope { seq, ts, body };
                command_tx.send(envelope).await.expect("queue has room");
            }

            let mut reference = SequencedMatcher::new();
            for (ts, body) in commands().into_iter().take(matched) {
                assert_eq!(next(&mut events).await, reference.apply(ts, body));
            }
            let payload = finish(thread).await.expect_err("the thread panics");
            let message = payload
                .downcast_ref::<String>()
                .expect("the panic has a formatted message");
            assert!(
                message.contains("gap or is out of order"),
                "{seqs:?}: {message}"
            );
            assert!(
                events.recv().await.is_none(),
                "{seqs:?}: the out-of-sequence command is not matched"
            );
        }
    }
}
