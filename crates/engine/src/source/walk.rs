//! Seeded random-walk order flow over a small fake book.

use std::time::Duration;

use protocol::{Envelope, Event, OrderId, Price, Qty, Seq, Side, Ts};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

use super::book::{Order, RestingBook};

/// Starting mid price in ticks.
const START_MID: Price = 100_000;
/// Each side is topped up to this many orders before anything else happens,
/// which also keeps both sides non-empty.
const MIN_ORDERS_PER_SIDE: usize = 20;
/// Above this many resting orders, cancels outweigh new orders, which keeps
/// the book near a realistic size instead of growing without bound.
const TARGET_ORDERS: usize = 300;
/// Furthest a new order rests from the mid, in ticks.
const MAX_OFFSET: Price = 60;
/// Largest order, in lots (0.5 BTC at five quantity decimals).
const MAX_QTY: Qty = 50_000;
/// A step moves the mid by one tick with probability 1 in this many.
const DRIFT_ODDS: u32 = 4;
/// Largest price change of a replace, in ticks.
const MAX_REPLACE_SHIFT: Price = 3;
/// Venue-time gap between two steps, in microseconds.
const STEP_GAP_US: std::ops::RangeInclusive<Ts> = 2_000..=10_000;

/// Events of one step and the venue time that passes before the next one.
#[derive(Debug)]
pub struct Step {
    /// Events in emission order, sequenced and timestamped.
    pub events: Vec<Envelope<Event>>,
    /// How long a real-time consumer waits before asking for the next step.
    pub wait: Duration,
}

/// A reproducible stream of engine events: limit orders placed around a
/// drifting mid, some canceled or replaced, and occasional takers.
///
/// The same seed and start time always produce the same events. The stream
/// keeps the invariants a real engine would: best bid below best ask, no
/// empty resting order, every `L2Update` equal to the level's true total and
/// trades only at the maker's price.
#[derive(Debug)]
pub struct RandomWalk {
    rng: ChaCha8Rng,
    book: RestingBook,
    mid: Price,
    next_order_id: OrderId,
    next_seq: Seq,
    clock: Ts,
}

impl RandomWalk {
    /// Creates a walk whose venue clock starts at `start_ts` microseconds.
    pub fn new(seed: u64, start_ts: Ts) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
            book: RestingBook::default(),
            mid: START_MID,
            next_order_id: 1,
            next_seq: 1,
            clock: start_ts,
        }
    }

    /// Advances the venue clock and produces the events of one action.
    pub fn step(&mut self) -> Step {
        let gap = self.rng.random_range(STEP_GAP_US);
        self.clock += gap;
        self.drift_mid();

        let mut out = Vec::new();
        if self.book.order_count(Side::Buy) < MIN_ORDERS_PER_SIDE {
            self.place(Side::Buy, &mut out);
        } else if self.book.order_count(Side::Sell) < MIN_ORDERS_PER_SIDE {
            self.place(Side::Sell, &mut out);
        } else {
            self.random_action(&mut out);
        }
        Step {
            events: out,
            wait: Duration::from_micros(gap),
        }
    }

    /// Nudges the mid, then keeps it inside the spread: the reference for new
    /// orders then follows the touch that takers and cancels move, instead of
    /// wandering off and piling orders up against a clamp.
    fn drift_mid(&mut self) {
        if self.rng.random_range(0..DRIFT_ODDS) == 0 {
            self.mid += self.rng.random_range(-1..=1);
        }
        if let (Some(bid), Some(ask)) = (self.book.best(Side::Buy), self.book.best(Side::Sell)) {
            self.mid = self.mid.clamp(bid, ask);
        }
    }

    fn random_action(&mut self, out: &mut Vec<Envelope<Event>>) {
        let side = self.random_side();
        let place_below = if self.book.len() > TARGET_ORDERS {
            35
        } else {
            55
        };
        let roll = self.rng.random_range(0..100);
        if roll < place_below {
            self.place(side, out);
        } else if roll < 80 {
            self.cancel(out);
        } else if roll < 92 {
            self.replace(out);
        } else {
            self.take(side, out);
        }
    }

    fn random_side(&mut self) -> Side {
        if self.rng.random_range(0..2) == 0 {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn random_qty(&mut self) -> Qty {
        self.rng.random_range(1..=MAX_QTY)
    }

    fn emit(&mut self, out: &mut Vec<Envelope<Event>>, body: Event) {
        out.push(Envelope {
            seq: self.next_seq,
            ts: self.clock,
            body,
        });
        self.next_seq += 1;
    }

    fn emit_level(&mut self, out: &mut Vec<Envelope<Event>>, side: Side, price: Price) {
        let qty = self.book.total(side, price);
        self.emit(out, Event::L2Update { side, price, qty });
    }

    /// Clamps a price so that an order on `side` would not cross the book.
    fn passive(&self, side: Side, price: Price) -> Price {
        match side {
            Side::Buy => self
                .book
                .best(Side::Sell)
                .map_or(price, |ask| price.min(ask - 1)),
            Side::Sell => self
                .book
                .best(Side::Buy)
                .map_or(price, |bid| price.max(bid + 1)),
        }
    }

    /// Rests a new order near the mid; nearer prices are more likely.
    fn place(&mut self, side: Side, out: &mut Vec<Envelope<Event>>) {
        let near = self.rng.random_range(1..=MAX_OFFSET);
        let offset = near.min(self.rng.random_range(1..=MAX_OFFSET));
        let wanted = match side {
            Side::Buy => self.mid - offset,
            Side::Sell => self.mid + offset,
        };
        let price = self.passive(side, wanted);
        let qty = self.random_qty();
        let order_id = self.next_order_id;
        self.next_order_id += 1;

        self.book.insert(order_id, Order { side, price, qty });
        self.emit(
            out,
            Event::Accepted {
                order_id,
                side,
                price,
                qty,
            },
        );
        self.emit_level(out, side, price);
    }

    fn random_order(&mut self) -> Option<(OrderId, Order)> {
        let n = self.rng.random_range(0..self.book.len().max(1));
        let id = self.book.nth_id(n)?;
        Some((id, self.book.get(id)?))
    }

    /// Cancels a random order, unless that would thin its side below the
    /// minimum depth; then it adds liquidity instead.
    fn cancel(&mut self, out: &mut Vec<Envelope<Event>>) {
        let Some((id, order)) = self.random_order() else {
            return;
        };
        if self.book.order_count(order.side) <= MIN_ORDERS_PER_SIDE {
            return self.place(order.side, out);
        }
        self.book.remove(id);
        self.emit(
            out,
            Event::Canceled {
                order_id: id,
                leaves_qty: order.qty,
            },
        );
        self.emit_level(out, order.side, order.price);
    }

    /// Gives a random order a new quantity and possibly a new price.
    fn replace(&mut self, out: &mut Vec<Envelope<Event>>) {
        let Some((id, order)) = self.random_order() else {
            return;
        };
        let shift = self
            .rng
            .random_range(-MAX_REPLACE_SHIFT..=MAX_REPLACE_SHIFT);
        let price = self.passive(order.side, order.price + shift);
        let qty = self.random_qty();
        self.apply_replace(id, order, price, qty, out);
    }

    /// Applies a replace. A new price or a larger quantity sends the order to
    /// the back of its level's queue; a smaller quantity keeps its place, as
    /// at production venues.
    fn apply_replace(
        &mut self,
        id: OrderId,
        order: Order,
        price: Price,
        qty: Qty,
        out: &mut Vec<Envelope<Event>>,
    ) {
        if price == order.price && qty <= order.qty {
            self.book.resize(id, qty);
        } else {
            self.book.remove(id);
            self.book.insert(
                id,
                Order {
                    price,
                    qty,
                    ..order
                },
            );
        }
        self.emit(
            out,
            Event::Replaced {
                order_id: id,
                price,
                qty,
            },
        );
        if price != order.price {
            self.emit_level(out, order.side, order.price);
        }
        self.emit_level(out, order.side, price);
    }

    /// A taker of `side` buys or sells part or all of the opposite touch
    /// level, oldest maker first. Without a second level behind it the touch
    /// is left alone and liquidity is added instead.
    fn take(&mut self, side: Side, out: &mut Vec<Envelope<Event>>) {
        let maker_side = opposite(side);
        let Some(price) = self.book.best(maker_side) else {
            return self.place(side, out);
        };
        if self.book.level_count(maker_side) < 2 {
            return self.place(side, out);
        }
        // The taker is a marketable limit order priced at the touch.
        let total = self.book.total(maker_side, price);
        let taker_qty = self.rng.random_range(1..=total);
        let taker_id = self.next_order_id;
        self.next_order_id += 1;
        self.emit(
            out,
            Event::Accepted {
                order_id: taker_id,
                side,
                price,
                qty: taker_qty,
            },
        );

        let mut remaining = taker_qty;
        for maker_id in self.book.queue(maker_side, price) {
            let Some(maker) = self.book.get(maker_id) else {
                break;
            };
            let qty = remaining.min(maker.qty);
            if qty == maker.qty {
                self.book.remove(maker_id);
            } else {
                self.book.resize(maker_id, maker.qty - qty);
            }
            self.emit(
                out,
                Event::Trade {
                    maker_order_id: maker_id,
                    taker_order_id: taker_id,
                    aggressor_side: side,
                    price,
                    qty,
                },
            );
            remaining -= qty;
            if remaining == 0 {
                break;
            }
        }
        self.emit_level(out, maker_side, price);
    }
}

fn opposite(side: Side) -> Side {
    match side {
        Side::Buy => Side::Sell,
        Side::Sell => Side::Buy,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// Rebuilds the book from events alone and checks every invariant on the way.
    #[derive(Default)]
    struct Shadow {
        orders: BTreeMap<OrderId, Order>,
        last_seq: Seq,
        last_ts: Ts,
    }

    impl Shadow {
        fn total(&self, side: Side, price: Price) -> Qty {
            self.orders
                .values()
                .filter(|o| o.side == side && o.price == price)
                .map(|o| o.qty)
                .sum()
        }

        fn best(&self, side: Side) -> Option<Price> {
            let prices = self
                .orders
                .values()
                .filter(|o| o.side == side)
                .map(|o| o.price);
            match side {
                Side::Buy => prices.max(),
                Side::Sell => prices.min(),
            }
        }

        fn apply(&mut self, env: &Envelope<Event>) {
            assert_eq!(env.seq, self.last_seq + 1, "seq must be gap-free");
            assert!(env.ts >= self.last_ts, "ts must not go backwards");
            (self.last_seq, self.last_ts) = (env.seq, env.ts);

            match env.body {
                Event::Accepted {
                    order_id,
                    side,
                    price,
                    qty,
                } => {
                    assert!(qty > 0);
                    let previous = self.orders.insert(order_id, Order { side, price, qty });
                    assert!(previous.is_none(), "order ids are unique");
                }
                Event::Trade {
                    maker_order_id,
                    taker_order_id,
                    aggressor_side,
                    price,
                    qty,
                } => {
                    let maker = self.orders[&maker_order_id];
                    assert_eq!(price, maker.price, "trades happen at the maker's price");
                    assert_ne!(maker.side, aggressor_side);
                    assert!(qty > 0 && qty <= maker.qty);
                    self.fill(maker_order_id, qty);
                    self.fill(taker_order_id, qty);
                }
                Event::Replaced {
                    order_id,
                    price,
                    qty,
                } => {
                    assert!(qty > 0);
                    let order = self
                        .orders
                        .get_mut(&order_id)
                        .expect("replaced order rests");
                    (order.price, order.qty) = (price, qty);
                }
                Event::Canceled {
                    order_id,
                    leaves_qty,
                } => {
                    let order = self.orders.remove(&order_id).expect("canceled order rests");
                    assert_eq!(order.qty, leaves_qty);
                }
                Event::L2Update { side, price, qty } => {
                    assert_eq!(
                        qty,
                        self.total(side, price),
                        "L2Update must equal the level total"
                    );
                }
            }
        }

        fn fill(&mut self, id: OrderId, qty: Qty) {
            let order = self.orders.get_mut(&id).expect("filled order exists");
            order.qty -= qty;
            if order.qty == 0 {
                self.orders.remove(&id);
            }
        }

        fn assert_uncrossed(&self) {
            let bid = self.best(Side::Buy).expect("bids never run dry");
            let ask = self.best(Side::Sell).expect("asks never run dry");
            assert!(bid < ask, "crossed book: {bid} >= {ask}");
        }
    }

    fn run(seed: u64, steps: usize) -> Vec<Envelope<Event>> {
        let mut walk = RandomWalk::new(seed, 1_700_000_000_000_000);
        let mut shadow = Shadow::default();
        let mut all = Vec::new();
        for _ in 0..steps {
            let step = walk.step();
            for env in &step.events {
                shadow.apply(env);
            }
            if shadow.orders.len() >= 2 * MIN_ORDERS_PER_SIDE {
                shadow.assert_uncrossed();
            }
            all.extend(step.events);
        }
        shadow.assert_uncrossed();
        all
    }

    #[test]
    fn invariants_hold_over_many_seeds() {
        for seed in 0..4 {
            run(seed, 5_000);
        }
    }

    #[test]
    fn every_kind_of_event_occurs() {
        let events = run(1, 5_000);
        let has = |f: fn(&Event) -> bool| events.iter().any(|e| f(&e.body));
        assert!(has(|e| matches!(e, Event::Accepted { .. })));
        assert!(has(|e| matches!(e, Event::Trade { .. })));
        assert!(has(|e| matches!(e, Event::Replaced { .. })));
        assert!(has(|e| matches!(e, Event::Canceled { .. })));
    }

    #[test]
    fn same_seed_same_stream() {
        assert_eq!(run(42, 2_000), run(42, 2_000));
        assert_ne!(run(42, 2_000), run(43, 2_000));
    }

    #[test]
    fn takers_fill_oldest_maker_first() {
        let mut walk = RandomWalk::new(5, 0);
        let mut checked = 0;
        for _ in 0..5_000 {
            let bids = walk
                .book
                .best(Side::Buy)
                .map(|p| walk.book.queue(Side::Buy, p));
            let asks = walk
                .book
                .best(Side::Sell)
                .map(|p| walk.book.queue(Side::Sell, p));
            let filled: Vec<(Side, OrderId)> = walk
                .step()
                .events
                .iter()
                .filter_map(|e| match e.body {
                    Event::Trade {
                        maker_order_id,
                        aggressor_side,
                        ..
                    } => Some((opposite(aggressor_side), maker_order_id)),
                    _ => None,
                })
                .collect();
            for side in [Side::Buy, Side::Sell] {
                let queue = if side == Side::Buy { &bids } else { &asks };
                let makers: Vec<OrderId> = filled
                    .iter()
                    .filter(|(s, _)| *s == side)
                    .map(|&(_, id)| id)
                    .collect();
                if let Some(queue) = queue {
                    assert_eq!(
                        makers,
                        queue[..makers.len()],
                        "makers must fill in arrival order"
                    );
                    checked += makers.len();
                }
            }
        }
        assert!(checked > 0, "no trades were exercised");
    }

    #[test]
    fn long_run_stays_bounded_and_anchored_to_the_touch() {
        let mut walk = RandomWalk::new(3, 0);
        for _ in 0..200_000 {
            walk.step();
            let (Some(bid), Some(ask)) = (walk.book.best(Side::Buy), walk.book.best(Side::Sell))
            else {
                continue;
            };
            assert!(
                walk.book.len() <= TARGET_ORDERS + 50,
                "book grew to {}",
                walk.book.len()
            );
            assert!(ask - bid <= 20, "spread widened to {}", ask - bid);
            // The touch moves within a step after the mid is re-anchored, so allow slack.
            assert!(
                (bid - MAX_OFFSET..=ask + MAX_OFFSET).contains(&walk.mid),
                "mid {} strayed from the touch {bid}/{ask}",
                walk.mid
            );
        }
    }

    #[test]
    fn replace_loses_queue_position_unless_quantity_shrinks() {
        let mut walk = RandomWalk::new(0, 0);
        let order = Order {
            side: Side::Buy,
            price: 100,
            qty: 10,
        };
        for id in [1, 2, 3] {
            walk.book.insert(id, order);
        }
        let mut out = Vec::new();

        walk.apply_replace(1, order, 100, 4, &mut out);
        assert_eq!(
            walk.book.queue(Side::Buy, 100),
            vec![1, 2, 3],
            "smaller keeps its place"
        );

        let shrunk = walk.book.get(1).expect("order rests");
        walk.apply_replace(1, shrunk, 100, 12, &mut out);
        assert_eq!(
            walk.book.queue(Side::Buy, 100),
            vec![2, 3, 1],
            "larger goes to the back"
        );

        walk.apply_replace(2, order, 101, 10, &mut out);
        assert_eq!(walk.book.queue(Side::Buy, 101), vec![2]);
        assert_eq!(walk.book.total(Side::Buy, 100), 22);
    }
}
