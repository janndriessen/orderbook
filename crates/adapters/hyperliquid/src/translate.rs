//! Translation of book snapshots and trades into protocol commands.
//!
//! The translator keeps a shadow [`Matcher`] and applies every command it
//! emits to it. The engine is deterministic, so the shadow book is exactly
//! what rests in the engine, and each snapshot can be turned into the
//! commands that move the engine's book onto the snapshot's levels.

use std::collections::BTreeMap;

use matching::{Matcher, OrderBook};
use protocol::{Command, Event, OrderId, OrderType, Price, Qty, Side, TimeInForce, Ts};

use crate::decimal::MAX_VALUE;
use crate::{BookSnapshot, Trade, TradeSide};

/// Largest order size the engine's gateway accepts, in lots. Defined here
/// because the adapter does not depend on the engine.
const MAX_ORDER_QTY: Qty = 100_000_000;

/// How far the engine's book is known to differ from the source's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Divergence {
    /// Post-only orders the shadow book rejected because they would have
    /// crossed a resting order that the snapshot did not cancel.
    pub rejected_makers: u64,
    /// Trade quantity, in lots, that the shadow book could not fill because
    /// too little rested at the trade's price or better.
    pub unfilled_taker_qty: u64,
    /// Lots of a taker order that filled at a price other than the trade's
    /// own, which happens when a stale shadow book holds better-priced orders.
    pub off_price_fill_qty: u64,
    /// Orders not emitted because the engine's gateway would refuse them: a
    /// quantity of zero or above its maximum order size, or a price outside
    /// its bounds.
    pub skipped_commands: u64,
}

/// Turns the source's trades and snapshots into commands for the engine.
///
/// Callers feed events in time order. A trade with a time at or before a
/// snapshot's must be fed before that snapshot, because the snapshot already
/// reflects it.
#[derive(Debug, Default)]
pub struct Translator {
    shadow: Matcher,
    last_id: OrderId,
    divergence: Divergence,
}

impl Translator {
    /// A translator whose shadow book is empty.
    pub fn new() -> Self {
        Self::default()
    }

    /// The counters of how far the engine's book has diverged from the source.
    pub fn divergence(&self) -> Divergence {
        self.divergence
    }

    /// One immediate-or-cancel limit order at the trade's price and size on
    /// the taker's side.
    pub fn on_trade(&mut self, trade: &Trade) -> Vec<(Ts, Command)> {
        let side = match trade.side {
            TradeSide::B => Side::Buy,
            TradeSide::A => Side::Sell,
        };
        let Some((command, events)) = self.place(side, trade.price, TimeInForce::Ioc, trade.qty)
        else {
            return Vec::new();
        };
        for event in events {
            match event {
                Event::Trade { price, qty, .. } if price != trade.price => {
                    self.divergence.off_price_fill_qty += qty;
                }
                Event::Canceled { leaves_qty, .. } => {
                    self.divergence.unfilled_taker_qty += leaves_qty;
                }
                _ => {}
            }
        }
        vec![(trade.ts, command)]
    }

    /// Commands that make the shadow book show the snapshot's levels.
    ///
    /// Cancels and replaces come first, so a post-only order never meets a
    /// quantity that is about to go: levels missing from the snapshot are
    /// canceled whole, levels that shrank lose their newest orders first, and
    /// levels that grew get one post-only order for the difference.
    pub fn on_snapshot(&mut self, snapshot: &BookSnapshot) -> Vec<(Ts, Command)> {
        let ts = snapshot.ts;
        let (mut removals, mut places) = (Vec::new(), Vec::new());
        for (side, levels) in [(Side::Buy, &snapshot.bids), (Side::Sell, &snapshot.asks)] {
            let mut wanted: BTreeMap<Price, Qty> = BTreeMap::new();
            for level in levels.iter().filter(|level| level.qty > 0) {
                let qty = wanted.entry(level.price).or_default();
                *qty = qty.saturating_add(level.qty);
            }
            let book = self.shadow.book();
            for (price, level) in book.levels(side) {
                let have = level.total();
                match wanted.remove(&price) {
                    None => {
                        removals
                            .extend(level.orders().map(|order_id| Command::Cancel { order_id }));
                    }
                    Some(want) if want < have => {
                        let orders: Vec<OrderId> = level.orders().collect();
                        removals.extend(shrink(book, &orders, have - want));
                    }
                    Some(want) if want > have => places.push((side, price, want - have)),
                    Some(_) => {}
                }
            }
            places.extend(wanted.into_iter().map(|(price, qty)| (side, price, qty)));
        }
        let mut out = Vec::new();
        for command in removals {
            self.shadow.apply(command);
            out.push((ts, command));
        }
        for (side, price, qty) in places {
            if let Some((command, events)) = self.place(side, price, TimeInForce::PostOnly, qty) {
                if events
                    .iter()
                    .any(|event| matches!(event, Event::Rejected { .. }))
                {
                    self.divergence.rejected_makers += 1;
                }
                out.push((ts, command));
            }
        }
        out
    }

    /// Applies a limit order to the shadow and returns it with the shadow's
    /// events, or `None` if the gateway would refuse it.
    fn place(
        &mut self,
        side: Side,
        price: Price,
        time_in_force: TimeInForce,
        qty: Qty,
    ) -> Option<(Command, Vec<Event>)> {
        let price_ok = u64::try_from(price).is_ok_and(|p| (1..=MAX_VALUE).contains(&p));
        if !(1..=MAX_ORDER_QTY).contains(&qty) || !price_ok {
            self.divergence.skipped_commands += 1;
            return None;
        }
        self.last_id += 1;
        let command = Command::Place {
            order_id: self.last_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force,
            },
            qty,
        };
        let events = self.shadow.apply(command);
        Some((command, events))
    }
}

/// Commands that remove `excess` lots from a level, newest orders first. An
/// order that is only partly removed is replaced with a smaller quantity.
fn shrink(book: &OrderBook, oldest_first: &[OrderId], mut excess: Qty) -> Vec<Command> {
    let mut commands = Vec::new();
    for &order_id in oldest_first.iter().rev() {
        let order = book.order(order_id).expect("every queued order is indexed");
        if order.qty > excess {
            commands.push(Command::Replace {
                order_id,
                price: order.price,
                qty: order.qty - excess,
            });
            break;
        }
        excess -= order.qty;
        commands.push(Command::Cancel { order_id });
        if excess == 0 {
            break;
        }
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BookLevel;

    fn snap(ts: Ts, bids: &[(Price, Qty)], asks: &[(Price, Qty)]) -> BookSnapshot {
        let levels = |side: &[(Price, Qty)]| {
            side.iter()
                .map(|&(price, qty)| BookLevel {
                    price,
                    qty,
                    orders: 1,
                })
                .collect()
        };
        BookSnapshot {
            ts,
            bids: levels(bids),
            asks: levels(asks),
        }
    }

    fn trade(ts: Ts, side: TradeSide, price: Price, qty: Qty) -> Trade {
        Trade {
            ts,
            price,
            qty,
            side,
            tid: 1,
        }
    }

    fn post(order_id: OrderId, side: Side, price: Price, qty: Qty) -> Command {
        Command::Place {
            order_id,
            side,
            order_type: OrderType::Limit {
                price,
                time_in_force: TimeInForce::PostOnly,
            },
            qty,
        }
    }

    fn commands(out: Vec<(Ts, Command)>) -> Vec<Command> {
        out.into_iter().map(|(_, command)| command).collect()
    }

    fn level_ids(t: &Translator, side: Side, price: Price) -> Vec<OrderId> {
        t.shadow
            .book()
            .level(side, price)
            .map_or_else(Vec::new, |l| l.orders().collect())
    }

    fn levels_of(t: &Translator, side: Side) -> Vec<(Price, Qty)> {
        t.shadow
            .book()
            .levels(side)
            .map(|(price, l)| (price, l.total()))
            .collect()
    }

    /// A translator whose bid level at 100 holds orders 1, 2, 3 of 4, 3, 2.
    fn with_three_bids() -> Translator {
        let mut t = Translator::new();
        for qty in [4, 7, 9] {
            t.on_snapshot(&snap(1, &[(100, qty)], &[]));
        }
        t
    }

    #[test]
    fn first_snapshot_posts_one_order_per_level() {
        let mut t = Translator::new();
        let out = t.on_snapshot(&snap(7, &[(100, 5), (99, 2)], &[(101, 3)]));
        assert!(out.iter().all(|&(ts, _)| ts == 7));
        assert_eq!(
            commands(out),
            [
                post(1, Side::Buy, 99, 2),
                post(2, Side::Buy, 100, 5),
                post(3, Side::Sell, 101, 3)
            ]
        );
        assert_eq!(t.divergence(), Divergence::default());
    }

    #[test]
    fn a_grown_level_gets_one_order_for_the_difference() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[(100, 5)], &[]));
        let out = t.on_snapshot(&snap(2, &[(100, 8)], &[]));
        assert_eq!(commands(out), [post(2, Side::Buy, 100, 3)]);
    }

    #[test]
    fn a_shrunk_level_loses_its_newest_orders_first() {
        let mut t = with_three_bids();
        let out = t.on_snapshot(&snap(2, &[(100, 6)], &[]));
        assert_eq!(
            commands(out),
            [
                Command::Cancel { order_id: 3 },
                Command::Replace {
                    order_id: 2,
                    price: 100,
                    qty: 2
                }
            ]
        );
        assert_eq!(level_ids(&t, Side::Buy, 100), [1, 2]);
        assert_eq!(
            t.shadow.book().level(Side::Buy, 100).map(|l| l.total()),
            Some(6)
        );
        assert_eq!(t.shadow.book().order(1).map(|o| o.qty), Some(4));
    }

    #[test]
    fn a_level_missing_from_the_snapshot_is_canceled_whole() {
        let mut t = with_three_bids();
        let out = t.on_snapshot(&snap(2, &[(99, 1)], &[]));
        assert_eq!(
            commands(out),
            [
                Command::Cancel { order_id: 1 },
                Command::Cancel { order_id: 2 },
                Command::Cancel { order_id: 3 },
                post(4, Side::Buy, 99, 1)
            ]
        );
        assert!(level_ids(&t, Side::Buy, 100).is_empty());
        let out = t.on_snapshot(&snap(3, &[(99, 0)], &[]));
        assert_eq!(commands(out), [Command::Cancel { order_id: 4 }]);
    }

    #[test]
    fn cancels_and_replaces_precede_places() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[(100, 5), (99, 5)], &[(101, 5)]));
        let out = commands(t.on_snapshot(&snap(2, &[(100, 7), (98, 1)], &[(101, 2), (102, 1)])));
        let first_place = out
            .iter()
            .position(|c| matches!(c, Command::Place { .. }))
            .unwrap();
        assert_eq!(first_place, 2);
        assert!(
            out[first_place..]
                .iter()
                .all(|c| matches!(c, Command::Place { .. }))
        );
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn a_trade_that_fills_resting_orders_is_already_in_the_next_snapshot() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[(99, 5)], &[(101, 5)]));
        let out = t.on_trade(&trade(2, TradeSide::B, 101, 2));
        assert_eq!(out.len(), 1);
        assert_eq!(t.divergence(), Divergence::default());
        assert!(t.on_snapshot(&snap(3, &[(99, 5)], &[(101, 3)])).is_empty());
    }

    #[test]
    fn a_trade_where_nothing_rests_is_counted_as_unfilled() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[], &[(101, 2)]));
        t.on_trade(&trade(2, TradeSide::B, 101, 5));
        t.on_trade(&trade(3, TradeSide::A, 90, 4));
        assert_eq!(t.divergence().unfilled_taker_qty, 3 + 4);
    }

    #[test]
    fn a_crossing_snapshot_counts_a_rejected_maker() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[], &[(100, 5)]));
        let out = t.on_snapshot(&snap(2, &[(100, 3)], &[(100, 5)]));
        assert_eq!(commands(out), [post(2, Side::Buy, 100, 3)]);
        assert_eq!(t.divergence().rejected_makers, 1);
        assert!(level_ids(&t, Side::Buy, 100).is_empty());
    }

    fn cancel(order_id: OrderId) -> Command {
        Command::Cancel { order_id }
    }

    #[test]
    fn shrinking_on_an_order_boundary_emits_no_replace() {
        let mut t = with_three_bids();
        assert_eq!(
            commands(t.on_snapshot(&snap(2, &[(100, 7)], &[]))),
            [cancel(3)]
        );
        assert_eq!(
            commands(t.on_snapshot(&snap(3, &[(100, 4)], &[]))),
            [cancel(2)]
        );
        let mut t = with_three_bids();
        assert_eq!(
            commands(t.on_snapshot(&snap(2, &[(100, 4)], &[]))),
            [cancel(3), cancel(2)]
        );
    }

    #[test]
    fn empty_snapshots_cancel_everything_and_are_idempotent() {
        let mut t = Translator::new();
        assert!(t.on_snapshot(&snap(1, &[], &[])).is_empty());
        let full = snap(2, &[(100, 5), (99, 1)], &[(101, 3)]);
        t.on_snapshot(&full);
        assert!(t.on_snapshot(&full).is_empty());
        let out = commands(t.on_snapshot(&snap(3, &[], &[])));
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c| matches!(c, Command::Cancel { .. })));
        assert!(t.shadow.book().is_empty());
    }

    #[test]
    fn a_moved_ask_is_canceled_before_the_bid_that_would_cross_it() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[], &[(100, 5)]));
        let out = t.on_snapshot(&snap(2, &[(100, 3)], &[(101, 5)]));
        assert_eq!(
            commands(out),
            [
                cancel(1),
                post(2, Side::Buy, 100, 3),
                post(3, Side::Sell, 101, 5)
            ]
        );
        assert_eq!(t.divergence().rejected_makers, 0);
    }

    #[test]
    fn a_sell_trade_fills_resting_bids() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[(99, 5)], &[]));
        t.on_trade(&trade(2, TradeSide::A, 99, 2));
        assert_eq!(levels_of(&t, Side::Buy), [(99, 3)]);
        assert_eq!(t.divergence(), Divergence::default());
    }

    #[test]
    fn a_stale_shadow_fills_at_a_better_price_and_counts_it() {
        let mut t = Translator::new();
        t.on_snapshot(&snap(1, &[], &[(100, 5), (101, 5)]));
        t.on_trade(&trade(2, TradeSide::B, 101, 3));
        assert_eq!(levels_of(&t, Side::Sell), [(100, 2), (101, 5)]);
        assert_eq!(
            t.divergence(),
            Divergence {
                off_price_fill_qty: 3,
                ..Divergence::default()
            }
        );
    }

    #[test]
    fn gateway_bounds_are_enforced_exactly_at_the_edges() {
        let max_price = Price::try_from(MAX_VALUE).unwrap();
        let mut t = Translator::new();
        assert_eq!(
            t.on_snapshot(&snap(1, &[(100, MAX_ORDER_QTY)], &[])).len(),
            1
        );
        assert_eq!(
            t.on_snapshot(&snap(
                2,
                &[(100, MAX_ORDER_QTY), (99, MAX_ORDER_QTY + 1)],
                &[]
            ))
            .len(),
            0
        );
        assert_eq!(t.divergence().skipped_commands, 1);
        assert_eq!(
            t.on_snapshot(&snap(3, &[(100, MAX_ORDER_QTY), (max_price, 1)], &[]))
                .len(),
            1
        );
        assert_eq!(
            t.on_snapshot(&snap(
                4,
                &[
                    (100, MAX_ORDER_QTY),
                    (max_price, 1),
                    (max_price + 1, 1),
                    (0, 1)
                ],
                &[]
            ))
            .len(),
            0
        );
        assert_eq!(t.divergence().skipped_commands, 3);
        assert!(
            t.on_trade(&trade(5, TradeSide::A, 100, MAX_ORDER_QTY + 1))
                .is_empty()
        );
        assert!(t.on_trade(&trade(6, TradeSide::A, 100, 0)).is_empty());
        assert_eq!(t.divergence().skipped_commands, 5);
        assert_eq!(t.divergence().unfilled_taker_qty, 0);
        assert_eq!(
            t.on_snapshot(&snap(7, &[(100, MAX_ORDER_QTY), (max_price, 1)], &[]))
                .len(),
            0
        );
    }

    #[test]
    fn the_shadow_equals_every_snapshot_and_ids_increase() {
        let script = [
            snap(1, &[(100, 9), (99, 4)], &[(101, 6), (102, 2)]),
            snap(2, &[(100, 12), (99, 4)], &[(101, 6), (102, 2)]),
            snap(3, &[(100, 5), (99, 4)], &[(101, 3), (102, 2)]),
            snap(4, &[(100, 5), (98, 1)], &[(101, 3), (103, 2)]),
            snap(5, &[(100, 7), (99, 0)], &[]),
            snap(6, &[], &[(102, 5)]),
        ];
        let mut t = Translator::new();
        let mut ids = Vec::new();
        for (step, snapshot) in script.iter().enumerate() {
            let mut out = t.on_snapshot(snapshot);
            for (side, levels) in [(Side::Buy, &snapshot.bids), (Side::Sell, &snapshot.asks)] {
                let mut expected: Vec<_> = levels
                    .iter()
                    .filter(|l| l.qty > 0)
                    .map(|l| (l.price, l.qty))
                    .collect();
                expected.sort_unstable();
                assert_eq!(levels_of(&t, side), expected, "step {step}, {side:?}");
            }
            if step == 2 {
                out.extend(t.on_trade(&trade(3, TradeSide::B, 101, 2)));
                out.extend(t.on_trade(&trade(3, TradeSide::A, 100, 1)));
            }
            ids.extend(out.iter().filter_map(|(_, c)| match c {
                Command::Place { order_id, .. } => Some(*order_id),
                _ => None,
            }));
        }
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ids.first(), Some(&1));
        assert_eq!(t.divergence(), Divergence::default());
    }
}
