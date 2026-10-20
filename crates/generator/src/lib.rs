//! Seeded fuzz order flow: a never-ending stream of commands that tries to
//! break the order book.
//!
//! Every command stays inside the bounds the gateway enforces and the
//! matcher's preconditions, so it can be fed straight into a matcher: the
//! flow is hostile to the book's logic (crossing prices, empty sides, stale
//! and unknown ids), never to its input validation. It does not track book
//! state; besides its random number generator it only counts the ids it has
//! placed.

use protocol::{Command, OrderId, OrderType, Price, Qty, Side, TimeInForce};
use rand::{RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;

/// Largest quantity of any order or replace, in lots: small enough that
/// orders regularly consume each other.
const MAX_QTY: Qty = 50;
/// Price every order and replace is drawn around, in ticks.
const MID: Price = 100_000;
/// Furthest a price lies from `MID`, in ticks. Both sides reach past the mid,
/// so buys and sells cross each other often.
const MAX_OFFSET: Price = 10;
/// Cancels and replaces of a placed id name one of this many most recently
/// placed ids. Most of those orders have already traded or been canceled, so
/// these commands mostly provoke cancel rejects; the rest hit resting orders.
const RECENT_IDS: u64 = 40;
/// How far above the last placed id a never-placed id can be.
const UNKNOWN_ID_REACH: u64 = 1_000;

// The gateway refuses quantities above 10^8 lots and prices outside
// 1..=2^53-1. Stated here because this crate does not depend on the engine.
const _: () = assert!(MAX_QTY <= 100_000_000);
const _: () = assert!(MID - MAX_OFFSET > 0 && MID + MAX_OFFSET < 1 << 53);

/// An endless, reproducible stream of adversarial commands.
///
/// With the locked `rand` version, the same seed always produces the same
/// commands. Place ids start at 1 and increase by one, quantities are in
/// `1..=MAX_QTY` and prices lie within `MAX_OFFSET` of `MID`. A cancel or
/// replace names either a recently placed id or one above every id placed so
/// far.
#[derive(Debug)]
pub struct FuzzFlow {
    rng: ChaCha8Rng,
    next_order_id: OrderId,
}

impl FuzzFlow {
    /// Creates a flow; the same `seed` gives the same commands.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
            next_order_id: 1,
        }
    }

    fn side(&mut self) -> Side {
        if self.rng.random_bool(0.5) {
            Side::Buy
        } else {
            Side::Sell
        }
    }

    fn qty(&mut self) -> Qty {
        self.rng.random_range(1..=MAX_QTY)
    }

    /// A price on either side of the mid, so orders and replaces cross the
    /// spread about as often as they rest.
    fn price(&mut self) -> Price {
        MID + self.rng.random_range(-MAX_OFFSET..=MAX_OFFSET)
    }

    /// One of the most recently placed ids. At least one id has been placed.
    fn placed_id(&mut self) -> OrderId {
        let last = self.next_order_id - 1;
        let first = last.saturating_sub(RECENT_IDS - 1).max(1);
        self.rng.random_range(first..=last)
    }

    /// An id above every id placed so far. A later place may take it, which is
    /// fine: the order did not exist when it was named.
    fn unknown_id(&mut self) -> OrderId {
        self.next_order_id + self.rng.random_range(0..UNKNOWN_ID_REACH)
    }

    fn place(&mut self, order_type: OrderType) -> Command {
        let order_id = self.next_order_id;
        self.next_order_id += 1;
        Command::Place {
            order_id,
            side: self.side(),
            order_type,
            qty: self.qty(),
        }
    }

    fn limit(&mut self, time_in_force: TimeInForce) -> Command {
        let price = self.price();
        self.place(OrderType::Limit {
            price,
            time_in_force,
        })
    }

    /// Replaces draw a fresh price and quantity, so a replace moves the price
    /// across the spread or keeps it, and grows or shrinks the quantity.
    fn replace(&mut self, order_id: OrderId) -> Command {
        Command::Replace {
            order_id,
            price: self.price(),
            qty: self.qty(),
        }
    }
}

impl Iterator for FuzzFlow {
    type Item = Command;

    fn next(&mut self) -> Option<Command> {
        // Weights out of 100. Resting orders dominate so there is something to
        // cross, cancel and replace; every kind stays common enough to occur
        // many times in a few thousand commands.
        let command = match self.rng.random_range(0..100) {
            0..30 => self.limit(TimeInForce::Gtc),
            30..42 => self.limit(TimeInForce::Ioc),
            42..54 => self.limit(TimeInForce::PostOnly),
            54..62 => self.place(OrderType::Market {}),
            // Before the first place there is no id to cancel or replace.
            _ if self.next_order_id == 1 => self.limit(TimeInForce::Gtc),
            62..74 => Command::Cancel {
                order_id: self.placed_id(),
            },
            74..77 => Command::Cancel {
                order_id: self.unknown_id(),
            },
            77..95 => {
                let order_id = self.placed_id();
                self.replace(order_id)
            }
            _ => {
                let order_id = self.unknown_id();
                self.replace(order_id)
            }
        };
        Some(command)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use matching::Matcher;
    use protocol::{CancelRejectReason, Event, RejectReason};

    use super::*;

    const SEEDS: [u64; 4] = [1, 2, 3, 4];
    const COMMANDS: usize = 5_000;

    fn commands(seed: u64) -> Vec<Command> {
        FuzzFlow::new(seed).take(COMMANDS).collect()
    }

    #[test]
    fn the_seed_decides_the_commands() {
        assert_eq!(commands(7), commands(7));
        assert_ne!(commands(7), commands(8));
    }

    /// What a command aims at, given the last id placed before it.
    fn command_kind(command: Command, last_placed: OrderId) -> &'static str {
        let placed = |order_id| (1..=last_placed).contains(&order_id);
        match command {
            Command::Place {
                order_type: OrderType::Market {},
                ..
            } => "market",
            Command::Place {
                order_type: OrderType::Limit { time_in_force, .. },
                ..
            } => match time_in_force {
                TimeInForce::Gtc => "gtc",
                TimeInForce::Ioc => "ioc",
                TimeInForce::PostOnly => "post_only",
            },
            Command::Cancel { order_id } if placed(order_id) => "cancel_placed",
            Command::Cancel { order_id } if order_id > last_placed => "cancel_unknown",
            Command::Replace { order_id, .. } if placed(order_id) => "replace_placed",
            Command::Replace { order_id, .. } if order_id > last_placed => "replace_unknown",
            _ => panic!("{command:?} names neither a placed id nor an unknown one"),
        }
    }

    fn outcome(event: &Event) -> &'static str {
        match event {
            Event::Accepted { .. } => "accepted",
            Event::Trade { .. } => "trade",
            Event::Canceled { .. } => "canceled",
            Event::Replaced { .. } => "replaced",
            Event::L2Update { .. } => "l2_update",
            Event::Rejected {
                reason: RejectReason::PostOnlyWouldCross,
                ..
            } => "rejected_post_only",
            Event::Rejected {
                reason: RejectReason::NoLiquidity,
                ..
            } => "rejected_no_liquidity",
            Event::CancelRejected {
                reason: CancelRejectReason::UnknownOrder,
                ..
            } => "cancel_rejected_unknown",
            Event::CancelRejected {
                reason: CancelRejectReason::PostOnlyWouldCross,
                ..
            } => "cancel_rejected_post_only",
        }
    }

    fn assert_in_bounds(price: Price, qty: Qty) {
        assert!((MID - MAX_OFFSET..=MID + MAX_OFFSET).contains(&price));
        assert!((1..=MAX_QTY).contains(&qty));
    }

    /// Every command stays in bounds, the matcher never panics on the flow,
    /// and every kind of command and every outcome occurs. The outcomes are
    /// measured, not guaranteed: the rarest, a market order on an empty side,
    /// happens early in a run and not for every seed, but still about twenty
    /// times across these seeds, so a change that makes one vanish fails here.
    #[test]
    fn the_flow_stays_in_bounds_and_provokes_every_outcome() {
        let (mut kinds, mut outcomes) = (HashSet::new(), HashSet::new());
        for seed in SEEDS {
            let mut matcher = Matcher::new();
            let mut last_placed = 0;
            for command in commands(seed) {
                kinds.insert(command_kind(command, last_placed));
                match command {
                    Command::Place {
                        order_id,
                        order_type,
                        qty,
                        ..
                    } => {
                        assert_eq!(order_id, last_placed + 1, "place ids are consecutive");
                        last_placed = order_id;
                        let price = match order_type {
                            OrderType::Limit { price, .. } => price,
                            OrderType::Market {} => MID,
                        };
                        assert_in_bounds(price, qty);
                    }
                    Command::Replace { price, qty, .. } => assert_in_bounds(price, qty),
                    Command::Cancel { .. } => {}
                }
                outcomes.extend(matcher.apply(command).iter().map(outcome));
            }
        }
        for kind in [
            "gtc",
            "ioc",
            "post_only",
            "market",
            "cancel_placed",
            "cancel_unknown",
            "replace_placed",
            "replace_unknown",
        ] {
            assert!(kinds.contains(kind), "no {kind} command");
        }
        for outcome in [
            "accepted",
            "trade",
            "canceled",
            "replaced",
            "l2_update",
            "rejected_post_only",
            "rejected_no_liquidity",
            "cancel_rejected_unknown",
            "cancel_rejected_post_only",
        ] {
            assert!(outcomes.contains(outcome), "no {outcome} event");
        }
    }
}
