//! The publisher's state machine, free of timers and tasks.
//!
//! One owner mutates it, so a snapshot is always taken between two ticks and
//! needs no locking. The async wrapper in the parent module only decides
//! when to call these methods.

use std::collections::VecDeque;
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use protocol::{Envelope, Event, FeedMessage, MarketInfo, Seq};
use tokio::sync::broadcast;

use super::TICK;
use super::book::AggregatedBook;

/// A serialized feed message. Cloning shares the bytes, so every client gets
/// the same allocation and the message is serialized once.
pub type Frame = Utf8Bytes;

/// How many recent events a snapshot carries to seed a client's flow log.
pub const RECENT_EVENTS: usize = 200;

/// Silence after which an idle feed sends a heartbeat, and the gap between
/// further ones. Clients time out after about 3.5 times this (see
/// `web/src/feed/FeedHandler.ts`), so change both together.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);

/// Idle ticks that make up one [`HEARTBEAT_INTERVAL`]. Counting ticks keeps
/// the state machine free of clocks.
const IDLE_TICKS_PER_HEARTBEAT: u32 = (HEARTBEAT_INTERVAL.as_millis() / TICK.as_millis()) as u32;

// A tick that does not divide the interval would heartbeat early or late, and
// one longer than the interval would heartbeat on every idle tick.
const _: () = assert!(
    TICK.as_millis() <= HEARTBEAT_INTERVAL.as_millis()
        && HEARTBEAT_INTERVAL
            .as_millis()
            .is_multiple_of(TICK.as_millis()),
    "TICK must divide HEARTBEAT_INTERVAL exactly"
);

/// Messages a client may fall behind by before it is disconnected: about six
/// seconds at the 100 ms tick.
pub const BROADCAST_CAPACITY: usize = 64;

/// A new client's starting point and its subscription to what follows.
#[derive(Debug)]
pub struct Subscription {
    /// Full state as of feed position `seq`; the next message is a heartbeat
    /// repeating `seq`, or the update `seq + 1`.
    pub snapshot: FeedMessage,
    /// Every message sent after the snapshot, and nothing sent before it.
    pub updates: broadcast::Receiver<Frame>,
}

/// Aggregates engine events into feed messages.
#[derive(Debug)]
pub struct Publisher {
    market: MarketInfo,
    book: AggregatedBook,
    /// Events received since the last tick. They reach neither the book nor
    /// `recent` until the tick that publishes them, so a snapshot never
    /// contains something the next update would deliver again.
    pending: Vec<Envelope<Event>>,
    recent: VecDeque<Envelope<Event>>,
    seq: Seq,
    /// Consecutive ticks that sent nothing.
    idle_ticks: u32,
    updates: broadcast::Sender<Frame>,
}

impl Publisher {
    /// Creates a publisher for an empty book at feed position 0.
    pub fn new(market: MarketInfo) -> Self {
        let (updates, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            market,
            book: AggregatedBook::default(),
            pending: Vec::new(),
            recent: VecDeque::with_capacity(RECENT_EVENTS),
            seq: 0,
            idle_ticks: 0,
            updates,
        }
    }

    /// Queues the events of one command for the next tick.
    ///
    /// They are taken together, so no tick can publish part of a command's
    /// events and leave the rest for the next update.
    pub fn ingest(&mut self, events: Vec<Envelope<Event>>) {
        self.pending.extend(events);
    }

    /// Publishes everything ingested since the last tick as one update.
    ///
    /// Returns the update's feed position, or `None` when nothing changed. An
    /// idle tick sends no update and consumes no position, so positions stay
    /// gap-free. After [`HEARTBEAT_INTERVAL`] of idle ticks it broadcasts a
    /// heartbeat that repeats the current position, and again after each
    /// further interval; clients tell loss from silence by `seq`.
    ///
    /// `L2Update` events only move the book. Their content reaches clients as
    /// level changes, so forwarding them would send everything twice; the
    /// flow log shows the other events. Forwarded events keep the engine's
    /// `seq`, which therefore skips the numbers of the `L2Update`s.
    pub fn tick(&mut self) -> Option<Seq> {
        let mut events = Vec::new();
        for envelope in std::mem::take(&mut self.pending) {
            match envelope.body {
                Event::L2Update { side, price, qty } => self.book.set(side, price, qty),
                _ => events.push(envelope),
            }
        }
        if events.is_empty() && !self.book.has_changes() {
            self.idle_ticks += 1;
            if self.idle_ticks >= IDLE_TICKS_PER_HEARTBEAT {
                self.idle_ticks = 0;
                let _ = self
                    .updates
                    .send(encode(&FeedMessage::Heartbeat { seq: self.seq }));
            }
            return None;
        }
        self.idle_ticks = 0;

        let (bids, asks) = self.book.take_changes();
        self.seq += 1;
        self.remember(&events);
        let update = FeedMessage::Update {
            seq: self.seq,
            bids,
            asks,
            events,
        };
        // No receivers is fine: the update is still part of the feed history.
        let _ = self.updates.send(encode(&update));
        Some(self.seq)
    }

    /// Snapshots the current state and subscribes to later updates.
    ///
    /// Both happen in this one call, between ticks, so the first frame the
    /// subscription yields is either a heartbeat repeating `snapshot.seq` or the
    /// update `snapshot.seq + 1`.
    pub fn subscribe(&self) -> Subscription {
        let (bids, asks) = self.book.levels();
        Subscription {
            snapshot: FeedMessage::Snapshot {
                seq: self.seq,
                market: self.market.clone(),
                bids,
                asks,
                events: self.recent.iter().cloned().collect(),
            },
            updates: self.updates.subscribe(),
        }
    }

    fn remember(&mut self, events: &[Envelope<Event>]) {
        self.recent.extend(events.iter().cloned());
        let excess = self.recent.len().saturating_sub(RECENT_EVENTS);
        self.recent.drain(..excess);
    }
}

/// Serializes a message for the wire.
pub fn encode(message: &FeedMessage) -> Frame {
    let json = serde_json::to_string(message)
        .expect("feed messages hold only strings, integers and lists, which always serialize");
    Frame::from(json)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use protocol::{
        CancelRejectReason, CancelRequest, Command, Level, OrderType, Price, Qty, RejectReason,
        Side, TimeInForce,
    };

    use super::*;
    use crate::app::market;
    use crate::matcher::SequencedMatcher;
    use crate::source::RandomWalk;

    fn env(seq: Seq, body: Event) -> Envelope<Event> {
        Envelope { seq, ts: 0, body }
    }

    fn l2(seq: Seq, side: Side, price: Price, qty: Qty) -> Envelope<Event> {
        env(seq, Event::L2Update { side, price, qty })
    }

    fn accepted(seq: Seq, order_id: u64) -> Envelope<Event> {
        env(
            seq,
            Event::Accepted {
                order_id,
                side: Side::Buy,
                order_type: OrderType::Limit {
                    price: 100,
                    time_in_force: TimeInForce::Gtc,
                },
                qty: 1,
            },
        )
    }

    fn next_update(rx: &mut broadcast::Receiver<Frame>) -> FeedMessage {
        let frame = rx.try_recv().expect("an update was sent");
        serde_json::from_str(frame.as_str()).expect("frames are feed messages")
    }

    fn lvl(price: Price, qty: Qty) -> Level {
        Level { price, qty }
    }

    #[test]
    fn update_carries_only_changed_levels_and_flow_events() {
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        let rejected = env(
            4,
            Event::Rejected {
                order_id: 8,
                reason: RejectReason::PostOnlyWouldCross,
            },
        );
        let cancel_rejected = env(
            5,
            Event::CancelRejected {
                order_id: 9,
                response_to: CancelRequest::Cancel,
                reason: CancelRejectReason::UnknownOrder,
            },
        );
        publisher.ingest(vec![accepted(1, 7)]);
        publisher.ingest(vec![l2(2, Side::Buy, 100, 5)]);
        publisher.ingest(vec![l2(3, Side::Sell, 101, 3)]);
        publisher.ingest(vec![rejected.clone()]);
        publisher.ingest(vec![cancel_rejected.clone()]);
        assert_eq!(publisher.tick(), Some(1));

        let FeedMessage::Update {
            seq,
            bids,
            asks,
            events,
        } = next_update(&mut rx)
        else {
            panic!("expected an update");
        };
        assert_eq!((seq, bids, asks), (1, vec![lvl(100, 5)], vec![lvl(101, 3)]));
        assert_eq!(
            events,
            vec![accepted(1, 7), rejected, cancel_rejected],
            "L2Update must not be forwarded"
        );

        publisher.ingest(vec![l2(6, Side::Buy, 100, 0)]);
        assert_eq!(publisher.tick(), Some(2));
        let FeedMessage::Update { bids, asks, .. } = next_update(&mut rx) else {
            panic!("expected an update");
        };
        assert_eq!((bids, asks), (vec![lvl(100, 0)], vec![]));
    }

    #[test]
    fn idle_ticks_send_nothing_and_keep_seq() {
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        assert_eq!(publisher.tick(), None);
        publisher.ingest(vec![l2(1, Side::Buy, 100, 1)]);
        assert_eq!(publisher.tick(), Some(1));
        assert_eq!(publisher.tick(), None);
        publisher.ingest(vec![l2(2, Side::Buy, 100, 2)]);
        assert_eq!(publisher.tick(), Some(2));
        assert!(matches!(
            next_update(&mut rx),
            FeedMessage::Update { seq: 1, .. }
        ));
        assert!(matches!(
            next_update(&mut rx),
            FeedMessage::Update { seq: 2, .. }
        ));
        assert!(rx.try_recv().is_err());
    }

    const SECOND: u32 = IDLE_TICKS_PER_HEARTBEAT;

    /// Runs `n` empty ticks and returns what reached the subscriber.
    fn idle(p: &mut Publisher, rx: &mut broadcast::Receiver<Frame>, n: u32) -> Vec<FeedMessage> {
        (0..n).for_each(|_| assert_eq!(p.tick(), None));
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|f| serde_json::from_str(f.as_str()).expect("valid frame"))
            .collect()
    }

    #[test]
    fn idle_ticks_send_one_heartbeat_per_second_without_moving_seq() {
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        assert_eq!(SECOND, 10, "one second at the 100 ms tick");
        let beat = |seq| vec![FeedMessage::Heartbeat { seq }];
        assert!(idle(&mut publisher, &mut rx, SECOND - 1).is_empty());
        assert_eq!(idle(&mut publisher, &mut rx, 1), beat(0));

        publisher.ingest(vec![accepted(1, 1)]);
        publisher.tick();
        let _ = next_update(&mut rx);
        for _ in 0..2 {
            assert!(idle(&mut publisher, &mut rx, SECOND - 1).is_empty());
            assert_eq!(idle(&mut publisher, &mut rx, 1), beat(1));
        }
    }

    #[test]
    fn a_busy_feed_sends_no_heartbeat() {
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        for seq in 1..=3 * u64::from(SECOND) {
            publisher.ingest(vec![accepted(seq, seq)]);
            assert_eq!(publisher.tick(), Some(seq));
            assert!(matches!(next_update(&mut rx), FeedMessage::Update { .. }));
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn an_update_restarts_the_idle_count() {
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        assert!(idle(&mut publisher, &mut rx, SECOND - 1).is_empty());
        publisher.ingest(vec![accepted(1, 1)]);
        publisher.tick();
        let _ = next_update(&mut rx);
        assert!(idle(&mut publisher, &mut rx, SECOND - 1).is_empty());
        let beat = idle(&mut publisher, &mut rx, 1);
        assert_eq!(beat, vec![FeedMessage::Heartbeat { seq: 1 }]);
    }

    #[test]
    fn a_commands_events_are_never_split_across_updates() {
        let mut matcher = SequencedMatcher::new();
        let mut publisher = Publisher::new(market());
        let mut rx = publisher.subscribe().updates;
        let place = |order_id, side, price, qty, time_in_force| Command::Place {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force,
            },
            qty,
        };
        for (id, price) in [(1, 101), (2, 102)] {
            let ask = place(id, Side::Sell, price, 1, TimeInForce::Gtc);
            publisher.ingest(matcher.apply(0, ask));
        }
        publisher.tick();
        next_update(&mut rx);

        let sweep = place(3, Side::Buy, 102, 2, TimeInForce::Ioc);
        publisher.ingest(matcher.apply(0, sweep));
        assert_eq!(publisher.tick(), Some(2));

        let FeedMessage::Update { asks, events, .. } = next_update(&mut rx) else {
            panic!("expected an update");
        };
        let trades = events
            .iter()
            .filter(|e| matches!(e.body, Event::Trade { .. }))
            .count();
        assert_eq!(trades, 2, "both trades arrive together");
        assert_eq!(asks, vec![lvl(101, 0), lvl(102, 0)], "with both levels");
        assert_eq!(publisher.tick(), None, "nothing of it is left for later");
    }

    #[test]
    fn snapshot_is_sorted_and_excludes_unticked_events() {
        let mut publisher = Publisher::new(market());
        for (i, (side, price)) in [
            (Side::Buy, 98),
            (Side::Buy, 100),
            (Side::Buy, 99),
            (Side::Sell, 102),
            (Side::Sell, 101),
        ]
        .into_iter()
        .enumerate()
        {
            publisher.ingest(vec![l2(i as u64 + 1, side, price, 1)]);
        }
        publisher.ingest(vec![accepted(6, 1)]);
        publisher.tick();
        publisher.ingest(vec![accepted(7, 2)]);
        publisher.ingest(vec![l2(8, Side::Buy, 97, 1)]);

        let FeedMessage::Snapshot {
            seq,
            market,
            bids,
            asks,
            events,
        } = publisher.subscribe().snapshot
        else {
            panic!("expected a snapshot");
        };
        assert_eq!(seq, 1);
        assert_eq!(market.symbol, "BTC-USD");
        assert_eq!(bids, vec![lvl(100, 1), lvl(99, 1), lvl(98, 1)]);
        assert_eq!(asks, vec![lvl(101, 1), lvl(102, 1)]);
        assert_eq!(events, vec![accepted(6, 1)]);
    }

    #[test]
    fn recent_events_are_capped_oldest_dropped() {
        let mut publisher = Publisher::new(market());
        let total = RECENT_EVENTS as u64 + 10;
        for seq in 1..=total {
            publisher.ingest(vec![accepted(seq, seq)]);
        }
        publisher.tick();
        let FeedMessage::Snapshot { events, .. } = publisher.subscribe().snapshot else {
            panic!("expected a snapshot");
        };
        assert_eq!(events.len(), RECENT_EVENTS);
        assert_eq!(events.first().map(|e| e.seq), Some(11));
        assert_eq!(events.last().map(|e| e.seq), Some(total));
    }

    /// What a browser does with the feed.
    #[derive(Default)]
    struct Client {
        bids: BTreeMap<Price, Qty>,
        asks: BTreeMap<Price, Qty>,
        seq: Seq,
        event_seqs: Vec<Seq>,
        heartbeats: u32,
    }

    impl Client {
        fn apply_levels(book: &mut BTreeMap<Price, Qty>, levels: &[Level], replace: bool) {
            if replace {
                book.clear();
            }
            for level in levels {
                if level.qty == 0 {
                    book.remove(&level.price);
                } else {
                    book.insert(level.price, level.qty);
                }
            }
        }

        fn apply(&mut self, message: FeedMessage) {
            match message {
                FeedMessage::Snapshot {
                    seq,
                    bids,
                    asks,
                    events,
                    ..
                } => {
                    Self::apply_levels(&mut self.bids, &bids, true);
                    Self::apply_levels(&mut self.asks, &asks, true);
                    self.seq = seq;
                    self.event_seqs = events.iter().map(|e| e.seq).collect();
                }
                FeedMessage::Update {
                    seq,
                    bids,
                    asks,
                    events,
                } => {
                    assert_eq!(seq, self.seq + 1, "gap or duplicate in the feed");
                    Self::apply_levels(&mut self.bids, &bids, false);
                    Self::apply_levels(&mut self.asks, &asks, false);
                    self.seq = seq;
                    self.event_seqs.extend(events.iter().map(|e| e.seq));
                }
                FeedMessage::Heartbeat { seq } => {
                    assert_eq!(seq, self.seq, "a heartbeat must not advance the feed");
                    self.heartbeats += 1;
                }
            }
        }
    }

    /// Delivers every queued frame, as clients do between ticks; the
    /// broadcast buffer is far smaller than a whole run.
    fn drain(clients: &mut [(Client, broadcast::Receiver<Frame>)]) {
        for (client, rx) in clients {
            while let Ok(frame) = rx.try_recv() {
                client.apply(serde_json::from_str(frame.as_str()).expect("valid frame"));
            }
        }
    }

    #[test]
    fn clients_joining_at_any_point_stay_consistent() {
        let mut walk = RandomWalk::new(11, 0);
        let mut matcher = SequencedMatcher::new();
        let mut publisher = Publisher::new(market());
        let mut flow_seqs = Vec::new();
        let mut clients: Vec<(Client, broadcast::Receiver<Frame>)> = Vec::new();
        let mut in_last_spell = 0;

        for i in 0..600 {
            let step = walk.step();
            let events = matcher.apply(step.ts, step.command);
            flow_seqs.extend(
                events
                    .iter()
                    .filter(|e| !matches!(e.body, Event::L2Update { .. }))
                    .map(|e| e.seq),
            );
            publisher.ingest(events);
            // Joins land before ticks, after ticks and with events pending.
            if i % 2 == 0 {
                let Subscription { snapshot, updates } = publisher.subscribe();
                let mut client = Client::default();
                client.apply(snapshot);
                clients.push((client, updates));
            }
            if i % 3 == 0 {
                publisher.tick();
                drain(&mut clients);
            }
            if i % 50 == 0 {
                // A quiet spell with heartbeats, joined by a client mid-way.
                publisher.tick();
                for tick in 0..2 * SECOND {
                    if tick == SECOND / 2 {
                        let Subscription { snapshot, updates } = publisher.subscribe();
                        let mut client = Client::default();
                        client.apply(snapshot);
                        clients.push((client, updates));
                    }
                    publisher.tick();
                }
                drain(&mut clients);
                in_last_spell = clients.len();
            }
        }
        publisher.tick();
        drain(&mut clients);

        let (bids, asks) = publisher.book.levels();
        let expected = |levels: Vec<Level>| -> BTreeMap<Price, Qty> {
            levels.into_iter().map(|l| (l.price, l.qty)).collect()
        };
        for (n, (client, _)) in clients.into_iter().enumerate() {
            if n < in_last_spell {
                assert!(client.heartbeats > 0, "client {n} saw no heartbeat");
            }
            assert_eq!(client.seq, publisher.seq);
            assert_eq!(client.bids, expected(bids.clone()));
            assert_eq!(client.asks, expected(asks.clone()));
            // The flow log is a gap-free, duplicate-free suffix of everything emitted.
            let start = flow_seqs.len() - client.event_seqs.len();
            assert_eq!(client.event_seqs, flow_seqs[start..]);
        }
    }
}
