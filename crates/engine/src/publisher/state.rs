//! The publisher's state machine, free of timers and tasks.
//!
//! One owner mutates it, so a snapshot is always taken between two ticks and
//! needs no locking. The async wrapper in the parent module only decides
//! when to call these methods.

use std::collections::VecDeque;

use axum::extract::ws::Utf8Bytes;
use protocol::{Envelope, Event, FeedMessage, MarketInfo, Seq};
use tokio::sync::broadcast;

use super::book::AggregatedBook;

/// A serialized feed message. Cloning shares the bytes, so every client gets
/// the same allocation and the message is serialized once.
pub type Frame = Utf8Bytes;

/// How many recent events a snapshot carries to seed a client's flow log.
pub const RECENT_EVENTS: usize = 200;

/// Updates a client may fall behind by before it is disconnected: about six
/// seconds at the 100 ms tick.
pub const BROADCAST_CAPACITY: usize = 64;

/// A new client's starting point and its subscription to what follows.
#[derive(Debug)]
pub struct Subscription {
    /// Full state as of feed position `seq`; the next frame is `seq + 1`.
    pub snapshot: FeedMessage,
    /// Every update sent after the snapshot, and nothing sent before it.
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
            updates,
        }
    }

    /// Queues an engine event for the next tick.
    pub fn ingest(&mut self, event: Envelope<Event>) {
        self.pending.push(event);
    }

    /// Publishes everything ingested since the last tick as one update.
    ///
    /// Returns the update's feed position, or `None` when nothing changed. An
    /// idle tick sends nothing and consumes no position, so positions stay
    /// gap-free and clients tell loss from silence by `seq` alone.
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
            return None;
        }

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
    /// subscription yields is exactly `snapshot.seq + 1`.
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

    use protocol::{Level, Price, Qty, Side};

    use super::*;
    use crate::app::market;
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
                price: 100,
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
        publisher.ingest(accepted(1, 7));
        publisher.ingest(l2(2, Side::Buy, 100, 5));
        publisher.ingest(l2(3, Side::Sell, 101, 3));
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
            vec![accepted(1, 7)],
            "L2Update must not be forwarded"
        );

        publisher.ingest(l2(4, Side::Buy, 100, 0));
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
        publisher.ingest(l2(1, Side::Buy, 100, 1));
        assert_eq!(publisher.tick(), Some(1));
        assert_eq!(publisher.tick(), None);
        publisher.ingest(l2(2, Side::Buy, 100, 2));
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
            publisher.ingest(l2(i as u64 + 1, side, price, 1));
        }
        publisher.ingest(accepted(6, 1));
        publisher.tick();
        publisher.ingest(accepted(7, 2));
        publisher.ingest(l2(8, Side::Buy, 97, 1));

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
            publisher.ingest(accepted(seq, seq));
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
        let mut publisher = Publisher::new(market());
        let mut flow_seqs = Vec::new();
        let mut clients: Vec<(Client, broadcast::Receiver<Frame>)> = Vec::new();

        for i in 0..600 {
            for envelope in walk.step().events {
                if !matches!(envelope.body, Event::L2Update { .. }) {
                    flow_seqs.push(envelope.seq);
                }
                publisher.ingest(envelope);
            }
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
        }
        publisher.tick();
        drain(&mut clients);

        let (bids, asks) = publisher.book.levels();
        let expected = |levels: Vec<Level>| -> BTreeMap<Price, Qty> {
            levels.into_iter().map(|l| (l.price, l.qty)).collect()
        };
        for (client, _) in clients {
            assert_eq!(client.seq, publisher.seq);
            assert_eq!(client.bids, expected(bids.clone()));
            assert_eq!(client.asks, expected(asks.clone()));
            // The flow log is a gap-free, duplicate-free suffix of everything emitted.
            let start = flow_seqs.len() - client.event_seqs.len();
            assert_eq!(client.event_seqs, flow_seqs[start..]);
        }
    }
}
