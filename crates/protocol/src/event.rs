//! Events: facts flowing out of the engine.

use serde::{Deserialize, Serialize};

use crate::{OrderId, OrderType, Price, Qty, Side, TradingStatus};

/// Something that happened in the engine as the result of a command.
///
/// One command can produce several events: an aggressive order produces
/// `Accepted`, one `Trade` per matched maker, `Canceled` if an
/// immediate-or-cancel or market remainder is left, and the resulting
/// `L2Update`s. A replace produces `Replaced`, one `Trade` per matched maker
/// if the new price crosses, and the resulting `L2Update`s; it is never
/// `Accepted` again.
///
/// The order is fixed. A new order gives `Accepted`, the `Trade`s, `Canceled`
/// for an unrested remainder, then one `L2Update` per touched level: the
/// consumed levels in consumption order, then the level it rests in. A replace
/// gives `Replaced`, the `Trade`s, then the old level, the consumed levels and
/// the new level if a remainder rests there. Each touched level is reported
/// once, even when the old and new level are the same. An
/// immediate-or-cancel order that trades nothing gives `Accepted` and
/// `Canceled` and no `L2Update`.
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
    /// The book is unchanged and the order is never `Accepted`. Refused
    /// cancels and replaces are reported with `CancelRejected`.
    ///
    /// A market order that trades at all is accepted and its remainder is
    /// canceled; only an empty opposite side rejects it.
    Rejected {
        /// The rejected order.
        order_id: OrderId,
        /// Why the order was refused.
        reason: RejectReason,
    },
    /// A cancel or replace request was refused.
    ///
    /// The order and the book are unchanged. `response_to` says which request
    /// was refused.
    CancelRejected {
        /// The order the request named.
        order_id: OrderId,
        /// The request that was refused.
        response_to: CancelRequest,
        /// Why the request was refused.
        reason: CancelRejectReason,
    },
    /// An order traded against a resting one.
    ///
    /// The taker is the incoming order, or a replaced order whose new price
    /// crossed the book.
    Trade {
        /// The resting order that provided liquidity.
        maker_order_id: OrderId,
        /// The order that took liquidity.
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
        /// Quantity requested by the replace. A replace that crosses the book
        /// reports its fills as `Trade`s, so the open quantity can end up
        /// smaller.
        qty: Qty,
    },
    /// An order, or its unfilled remainder, was canceled.
    ///
    /// An immediate-or-cancel or market order reports its remainder this way,
    /// possibly after partial fills.
    Canceled {
        /// The canceled order.
        order_id: OrderId,
        /// Quantity that was still open.
        leaves_qty: Qty,
    },
    /// The total resting quantity at one price level.
    ///
    /// Reported whenever a command touched the level: usually because the
    /// total changed, but a replace that changes nothing still reports its
    /// level with the unchanged total.
    L2Update {
        /// Side of the level.
        side: Side,
        /// Price of the level.
        price: Price,
        /// New total quantity at the level; zero means the level is gone.
        qty: Qty,
    },
    /// The trading status changed.
    ///
    /// Reported only when the status actually changed, as the sole event of
    /// its command. New feed clients learn the current status from the
    /// snapshot instead.
    TradingStatusChanged {
        /// The status now in force.
        status: TradingStatus,
    },
}

/// The kind of request a `CancelRejected` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum CancelRequest {
    /// A request to remove a resting order.
    Cancel,
    /// A request to change a resting order's price and quantity.
    Replace,
}

/// Why a cancel or replace request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum CancelRejectReason {
    /// No such resting order: never placed, already filled, or already canceled.
    UnknownOrder,
    /// A replace would move a post-only order to a price that trades on arrival.
    PostOnlyWouldCross,
    /// Trading is halted: a replace is refused while only cancels are accepted.
    TradingHalted,
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
    /// Trading is halted: only cancels are accepted.
    TradingHalted,
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
            (RejectReason::TradingHalted, "trading_halted"),
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
    fn cancel_rejected_has_a_fixed_wire_form() {
        for (response_to, request, reason, wire) in [
            (
                CancelRequest::Cancel,
                "cancel",
                CancelRejectReason::UnknownOrder,
                "unknown_order",
            ),
            (
                CancelRequest::Replace,
                "replace",
                CancelRejectReason::PostOnlyWouldCross,
                "post_only_would_cross",
            ),
            (
                CancelRequest::Replace,
                "replace",
                CancelRejectReason::TradingHalted,
                "trading_halted",
            ),
        ] {
            let event = Event::CancelRejected {
                order_id: 7,
                response_to,
                reason,
            };
            let expected = json!({
                "type": "cancel_rejected",
                "order_id": 7,
                "response_to": request,
                "reason": wire,
            });
            assert_eq!(serde_json::to_value(&event).unwrap(), expected);
            assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
        }
    }

    #[test]
    fn trading_status_changed_has_a_fixed_wire_form() {
        let event = Event::TradingStatusChanged {
            status: TradingStatus::Halted,
        };
        let expected = json!({ "type": "trading_status_changed", "status": "halted" });
        assert_eq!(serde_json::to_value(&event).unwrap(), expected);
        assert_eq!(serde_json::from_value::<Event>(expected).unwrap(), event);
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
