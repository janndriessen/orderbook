//! Events: facts flowing out of the engine.

use serde::{Deserialize, Serialize};

use crate::{OrderId, OrderType, Price, Qty, Side};

/// Something that happened in the engine as the result of a command.
///
/// One command can produce several events: an aggressive order produces
/// `Accepted`, one `Trade` per matched maker, and the resulting `L2Update`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum Event {
    /// An order was accepted.
    ///
    /// Carries the full order so that consumers such as the flow log need no
    /// lookup of their own.
    Accepted {
        /// The accepted order.
        order_id: OrderId,
        /// Buy or sell.
        side: Side,
        /// How the order is priced and how long it lives.
        order_type: OrderType,
        /// Quantity ordered.
        qty: Qty,
    },
    /// A new order was refused.
    ///
    /// The book is unchanged and the order is never `Accepted`. Refusals of
    /// cancels and replaces are not reported with this event.
    ///
    /// A market order that trades at all is accepted and its remainder is
    /// canceled; only an empty opposite side rejects it.
    Rejected {
        /// The rejected order.
        order_id: OrderId,
        /// Why the order was refused.
        reason: RejectReason,
    },
    /// An incoming order traded against a resting one.
    Trade {
        /// The resting order that provided liquidity.
        maker_order_id: OrderId,
        /// The incoming order that took liquidity.
        taker_order_id: OrderId,
        /// Side of the taker, which initiated the trade.
        aggressor_side: Side,
        /// Execution price: always the maker's price.
        price: Price,
        /// Quantity traded.
        qty: Qty,
    },
    /// A resting order's price or quantity was replaced.
    Replaced {
        /// The replaced order.
        order_id: OrderId,
        /// Price after the change.
        price: Price,
        /// Open quantity after the change.
        qty: Qty,
    },
    /// An order left the book without being filled.
    Canceled {
        /// The canceled order.
        order_id: OrderId,
        /// Quantity that was still open.
        leaves_qty: Qty,
    },
    /// The total resting quantity at one price level changed.
    L2Update {
        /// Side of the level.
        side: Side,
        /// Price of the level.
        price: Price,
        /// New total quantity at the level; zero means the level is gone.
        qty: Qty,
    },
}

/// Why an order was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum RejectReason {
    /// A post-only order would have traded on arrival.
    PostOnlyWouldCross,
    /// A market order found the opposite side of the book empty.
    NoLiquidity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TimeInForce;
    use serde_json::json;

    #[test]
    fn accepted_has_a_fixed_wire_form() {
        let event = Event::Accepted {
            order_id: 7,
            side: Side::Buy,
            order_type: OrderType::Limit {
                price: 100,
                time_in_force: TimeInForce::Gtc,
            },
            qty: 3,
        };
        let expected = json!({
            "type": "accepted",
            "order_id": 7,
            "side": "buy",
            "order_type": { "type": "limit", "price": 100, "time_in_force": "gtc" },
            "qty": 3,
        });
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
    }

    #[test]
    fn accepted_market_order_has_no_price() {
        let event = Event::Accepted {
            order_id: 8,
            side: Side::Sell,
            order_type: OrderType::Market {},
            qty: 2,
        };
        let expected = json!({
            "type": "accepted",
            "order_id": 8,
            "side": "sell",
            "order_type": { "type": "market" },
            "qty": 2,
        });
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
    }

    #[test]
    fn rejected_has_a_fixed_wire_form() {
        for (reason, wire) in [
            (RejectReason::PostOnlyWouldCross, "post_only_would_cross"),
            (RejectReason::NoLiquidity, "no_liquidity"),
        ] {
            let event = Event::Rejected {
                order_id: 7,
                reason,
            };
            let expected = json!({ "type": "rejected", "order_id": 7, "reason": wire });
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
            assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
        }
    }

    #[test]
    fn trade_round_trips() {
        let event = Event::Trade {
            maker_order_id: 1,
            taker_order_id: 2,
            aggressor_side: Side::Sell,
            price: 100_000,
            qty: 5,
        };
        let expected = json!({
            "type": "trade",
            "maker_order_id": 1,
            "taker_order_id": 2,
            "aggressor_side": "sell",
            "price": 100_000,
            "qty": 5,
        });
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
    }

    #[test]
    fn l2_update_tag_is_snake_case() {
        let event = Event::L2Update {
            side: Side::Buy,
            price: 99_999,
            qty: 0,
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({ "type": "l2_update", "side": "buy", "price": 99_999, "qty": 0 })
        );
    }
}
