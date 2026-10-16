//! Events: facts flowing out of the engine.

use serde::{Deserialize, Serialize};

use crate::{OrderId, Price, Qty, Side};

/// Something that happened in the engine as the result of a command.
///
/// One command can produce several events: an aggressive order produces
/// `Accepted`, one `Trade` per matched maker, and the resulting `L2Update`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum Event {
    /// A new order entered the book.
    ///
    /// Carries the full order so that consumers such as the flow log need no
    /// lookup of their own.
    Accepted {
        /// The accepted order.
        order_id: OrderId,
        /// Buy or sell.
        side: Side,
        /// Limit price.
        price: Price,
        /// Quantity ordered.
        qty: Qty,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
