//! The matcher with the event sequencing around it.

use matching::Matcher;
use protocol::{Command, Envelope, Event, Seq, Ts};

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

#[cfg(test)]
mod tests {
    use protocol::{OrderType, Side, TimeInForce};

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
}
