//! The gateway: checks that need no book state, and command sequencing in
//! front of the engine.
//!
//! It checks only what it can know without book state: the bounds of sizes,
//! prices and order ids, and order id reuse within a window. Whether an order
//! exists or would trade is the engine's call.

use std::collections::BTreeSet;

use protocol::{Command, Envelope, OrderId, OrderType, Price, Qty, Seq, Ts};

/// Largest order size in lots: 1,000 BTC at the market's five quantity
/// decimals (`qty_decimals` in `app::market`). Production venues cap order
/// size per product to stop fat-finger errors.
pub const MAX_ORDER_QTY: Qty = 100_000_000;

/// Largest limit price in ticks: the protocol's bound for values JavaScript
/// clients represent exactly.
pub const MAX_PRICE: Price = (1 << 53) - 1;

/// Largest order id. Order ids reach clients in the feed, so they stay within
/// the protocol's bound for values JavaScript represents exactly.
pub const MAX_ORDER_ID: OrderId = (1 << 53) - 1;

/// How many of the highest `Place` order ids the gateway remembers. Ids may
/// arrive somewhat out of order, so the window leaves room for a late one
/// while keeping memory fixed however long the service runs.
pub const ORDER_ID_WINDOW: usize = 1_000;

/// Why the gateway refused a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GatewayReject {
    /// The quantity is zero or above [`MAX_ORDER_QTY`].
    #[error("quantity {0} is outside 1..={MAX_ORDER_QTY}")]
    InvalidQty(Qty),
    /// The limit price is not in `1..=`[`MAX_PRICE`].
    #[error("price {0} is outside 1..={MAX_PRICE}")]
    InvalidPrice(Price),
    /// The order id is above [`MAX_ORDER_ID`].
    #[error("order id {0} is above {MAX_ORDER_ID}")]
    InvalidOrderId(OrderId),
    /// A `Place` already used this order id.
    #[error("order id {0} was already used")]
    DuplicateOrderId(OrderId),
    /// The window of remembered ids is full and this `Place` id is smaller
    /// than all of them, so it cannot be told apart from a reused one.
    #[error("order id {0} is below the window of the {ORDER_ID_WINDOW} highest ids")]
    OrderIdTooOld(OrderId),
}

/// Validates commands and numbers the ones that pass.
///
/// Command sequence numbers run gap-free from 1 over the accepted commands; a
/// reject consumes none.
///
/// Only the [`ORDER_ID_WINDOW`] highest `Place` order ids are remembered. A
/// `Place` passes the id check if its id is unused and, once the window is
/// full, larger than the smallest remembered id. Ids that fell out of the
/// window are refused rather than forgotten, so none is ever accepted twice.
#[derive(Debug, Default)]
pub struct Gateway {
    /// Sequence number of the last accepted command, 0 before the first.
    last_seq: Seq,
    /// The highest `Place` ids seen, at most [`ORDER_ID_WINDOW`] of them. A
    /// `Place` refused for its size or price is remembered and counts towards
    /// the window like any other: an id is unique once sent, and the only
    /// submitter is the service's own translator, so a stream of invalid
    /// `Place`s with huge ids that would push the floor up is not a case to
    /// guard against here.
    seen: BTreeSet<OrderId>,
}

impl Gateway {
    /// A gateway that has seen no commands.
    pub fn new() -> Self {
        Self::default()
    }

    /// Checks `command` and, if it passes, returns it with the next sequence
    /// number and `ts`, its venue time.
    ///
    /// Every command that names an order must use an id within
    /// [`MAX_ORDER_ID`]. Every `Place` uses up its order id, even one refused
    /// by a later check, because a client order id is unique once sent (within
    /// [`ORDER_ID_WINDOW`]). A
    /// `Replace` and a `Cancel` name an existing order, so the gateway does not
    /// check whether their ids were used; whether the order rests is for the
    /// engine to decide. A `SetTradingStatus` names no order and has no values
    /// to check; it is only sequenced.
    pub fn submit(&mut self, ts: Ts, command: Command) -> Result<Envelope<Command>, GatewayReject> {
        if let Command::Place { order_id, .. }
        | Command::Replace { order_id, .. }
        | Command::Cancel { order_id } = command
            && order_id > MAX_ORDER_ID
        {
            return Err(GatewayReject::InvalidOrderId(order_id));
        }
        match command {
            Command::Place {
                order_id,
                order_type,
                qty,
                ..
            } => {
                if self.seen.contains(&order_id) {
                    return Err(GatewayReject::DuplicateOrderId(order_id));
                }
                if self.seen.len() >= ORDER_ID_WINDOW && self.seen.first() > Some(&order_id) {
                    return Err(GatewayReject::OrderIdTooOld(order_id));
                }
                self.seen.insert(order_id);
                if self.seen.len() > ORDER_ID_WINDOW {
                    self.seen.pop_first();
                }
                check_qty(qty)?;
                if let OrderType::Limit { price, .. } = order_type {
                    check_price(price)?;
                }
            }
            Command::Replace { price, qty, .. } => {
                check_qty(qty)?;
                check_price(price)?;
            }
            Command::Cancel { .. } | Command::SetTradingStatus { .. } => {}
        }
        self.last_seq += 1;
        Ok(Envelope {
            seq: self.last_seq,
            ts,
            body: command,
        })
    }
}

fn check_qty(qty: Qty) -> Result<(), GatewayReject> {
    if (1..=MAX_ORDER_QTY).contains(&qty) {
        Ok(())
    } else {
        Err(GatewayReject::InvalidQty(qty))
    }
}

fn check_price(price: Price) -> Result<(), GatewayReject> {
    if (1..=MAX_PRICE).contains(&price) {
        Ok(())
    } else {
        Err(GatewayReject::InvalidPrice(price))
    }
}

#[cfg(test)]
mod tests {
    use protocol::{Side, TimeInForce, TradingStatus};

    use super::*;

    fn place(order_id: OrderId, price: Price, qty: Qty) -> Command {
        Command::Place {
            order_id,
            side: Side::Buy,
            order_type: OrderType::Limit {
                price,
                time_in_force: TimeInForce::Gtc,
            },
            qty,
        }
    }

    fn market(order_id: OrderId, qty: Qty) -> Command {
        Command::Place {
            order_id,
            side: Side::Sell,
            order_type: OrderType::Market {},
            qty,
        }
    }

    fn replace(order_id: OrderId, price: Price, qty: Qty) -> Command {
        Command::Replace {
            order_id,
            price,
            qty,
        }
    }

    /// Submits `command` to a fresh gateway.
    fn submit(command: Command) -> Result<Envelope<Command>, GatewayReject> {
        Gateway::new().submit(0, command)
    }

    #[test]
    fn set_trading_status_is_sequenced_without_further_checks() {
        let mut gateway = Gateway::new();
        let set = |status| Command::SetTradingStatus { status };
        let first = gateway.submit(5, set(TradingStatus::Halted)).unwrap();
        assert_eq!((first.seq, first.ts), (1, 5));
        let next = gateway.submit(6, place(1, 100, 1)).unwrap();
        assert_eq!(next.seq, 2);
        assert_eq!(
            gateway.submit(7, set(TradingStatus::Halted)).unwrap().seq,
            3
        );
    }

    #[test]
    fn place_quantity_must_be_between_one_and_the_maximum() {
        assert_eq!(submit(place(1, 100, 0)), Err(GatewayReject::InvalidQty(0)));
        assert!(submit(place(1, 100, 1)).is_ok());
        assert!(submit(place(1, 100, MAX_ORDER_QTY)).is_ok());
        assert_eq!(
            submit(place(1, 100, MAX_ORDER_QTY + 1)),
            Err(GatewayReject::InvalidQty(MAX_ORDER_QTY + 1))
        );
    }

    #[test]
    fn limit_price_must_be_between_one_and_the_maximum() {
        assert_eq!(
            submit(place(1, -1, 5)),
            Err(GatewayReject::InvalidPrice(-1))
        );
        assert_eq!(submit(place(1, 0, 5)), Err(GatewayReject::InvalidPrice(0)));
        assert!(submit(place(1, 1, 5)).is_ok());
        assert!(submit(place(1, MAX_PRICE, 5)).is_ok());
        assert_eq!(
            submit(place(1, MAX_PRICE + 1, 5)),
            Err(GatewayReject::InvalidPrice(MAX_PRICE + 1))
        );
    }

    #[test]
    fn market_order_has_no_price_to_check_but_its_quantity_is() {
        assert!(submit(market(1, 5)).is_ok());
        assert_eq!(submit(market(1, 0)), Err(GatewayReject::InvalidQty(0)));
    }

    #[test]
    fn reusing_a_place_order_id_is_rejected() {
        let mut gateway = Gateway::new();
        assert!(gateway.submit(1, place(7, 100, 5)).is_ok());
        assert_eq!(
            gateway.submit(2, market(7, 5)),
            Err(GatewayReject::DuplicateOrderId(7))
        );
    }

    /// Fills the window with ids `1..=ORDER_ID_WINDOW`.
    fn full_window() -> Gateway {
        let mut gateway = Gateway::new();
        for id in 1..=ORDER_ID_WINDOW as OrderId {
            assert!(gateway.submit(0, place(id, 100, 5)).is_ok());
        }
        gateway
    }

    #[test]
    fn an_id_below_the_full_window_is_too_old() {
        let mut gateway = full_window();
        // Id 1 is the smallest kept, so it is a duplicate; once a higher id
        // pushes it out, it is merely too old.
        assert_eq!(
            gateway.submit(0, place(1, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(1))
        );
        let next = ORDER_ID_WINDOW as OrderId + 1;
        assert!(gateway.submit(0, place(next, 100, 5)).is_ok());
        assert_eq!(
            gateway.submit(0, place(1, 100, 5)),
            Err(GatewayReject::OrderIdTooOld(1))
        );
        assert_eq!(
            gateway.submit(0, place(2, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(2))
        );
    }

    #[test]
    fn duplicates_inside_the_window_are_still_duplicates() {
        let mut gateway = full_window();
        assert_eq!(
            gateway.submit(0, place(500, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(500))
        );
    }

    #[test]
    fn an_unused_id_inside_the_window_is_accepted_out_of_order() {
        // Full window whose smallest kept id is 2: id 3 is unused and above it.
        let mut gateway = Gateway::new();
        for id in (2..=2 * ORDER_ID_WINDOW as OrderId).filter(|id| id % 2 == 0) {
            assert!(gateway.submit(0, place(id, 100, 5)).is_ok());
        }
        assert!(gateway.submit(0, place(3, 100, 5)).is_ok());
    }

    #[test]
    fn a_place_rejected_for_its_size_counts_towards_the_window() {
        let mut gateway = Gateway::new();
        for id in 1..ORDER_ID_WINDOW as OrderId {
            assert!(gateway.submit(0, place(id, 100, 5)).is_ok());
        }
        let last = ORDER_ID_WINDOW as OrderId;
        assert_eq!(
            gateway.submit(0, place(last, 100, 0)),
            Err(GatewayReject::InvalidQty(0))
        );
        // The rejected id filled the window, so an id below its smallest is
        // too old, and the rejected id itself is a duplicate.
        assert_eq!(
            gateway.submit(0, place(0, 100, 5)),
            Err(GatewayReject::OrderIdTooOld(0))
        );
        assert_eq!(
            gateway.submit(0, place(last, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(last))
        );
    }

    #[test]
    fn remembered_ids_never_exceed_the_window() {
        let mut gateway = Gateway::new();
        for id in 1..=3 * ORDER_ID_WINDOW as OrderId {
            gateway.submit(0, place(id, 100, 5)).unwrap();
            assert!(gateway.seen.len() <= ORDER_ID_WINDOW);
        }
        assert_eq!(gateway.seen.len(), ORDER_ID_WINDOW);
    }

    #[test]
    fn the_id_of_a_rejected_place_is_used_up() {
        let mut gateway = Gateway::new();
        assert_eq!(
            gateway.submit(1, place(7, 100, 0)),
            Err(GatewayReject::InvalidQty(0))
        );
        assert_eq!(
            gateway.submit(2, place(7, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(7))
        );
    }

    #[test]
    fn accepted_commands_are_numbered_from_one_and_a_reject_takes_no_number() {
        let mut gateway = Gateway::new();
        let first = gateway.submit(10, place(1, 100, 5)).unwrap();
        assert_eq!((first.seq, first.ts), (1, 10));
        assert_eq!(first.body, place(1, 100, 5));
        assert!(gateway.submit(20, place(2, 100, 0)).is_err());
        let second = gateway.submit(30, Command::Cancel { order_id: 1 }).unwrap();
        assert_eq!((second.seq, second.ts), (2, 30));
        let third = gateway.submit(30, place(3, 100, 5)).unwrap();
        assert_eq!((third.seq, third.ts), (3, 30));
    }

    #[test]
    fn order_id_must_not_exceed_the_maximum() {
        assert!(submit(place(MAX_ORDER_ID, 100, 5)).is_ok());
        let too_large = MAX_ORDER_ID + 1;
        for command in [
            place(too_large, 100, 5),
            replace(too_large, 100, 5),
            Command::Cancel {
                order_id: too_large,
            },
        ] {
            assert_eq!(
                submit(command),
                Err(GatewayReject::InvalidOrderId(too_large))
            );
        }
    }

    #[test]
    fn replace_and_cancel_of_a_placed_order_pass_and_keep_its_id_used() {
        let mut gateway = Gateway::new();
        assert!(gateway.submit(1, place(7, 100, 5)).is_ok());
        assert!(gateway.submit(2, replace(7, 101, 3)).is_ok());
        assert!(gateway.submit(3, Command::Cancel { order_id: 7 }).is_ok());
        assert_eq!(
            gateway.submit(4, place(7, 100, 5)),
            Err(GatewayReject::DuplicateOrderId(7))
        );
    }

    #[test]
    fn cancel_always_passes() {
        let envelope = submit(Command::Cancel { order_id: 99 }).unwrap();
        assert_eq!(envelope.body, Command::Cancel { order_id: 99 });
    }

    #[test]
    fn replace_needs_a_valid_quantity_and_price_but_not_a_known_id() {
        assert_eq!(
            submit(replace(99, 100, 0)),
            Err(GatewayReject::InvalidQty(0))
        );
        assert_eq!(
            submit(replace(99, 0, 5)),
            Err(GatewayReject::InvalidPrice(0))
        );
        assert!(submit(replace(99, 100, 5)).is_ok());
    }
}
