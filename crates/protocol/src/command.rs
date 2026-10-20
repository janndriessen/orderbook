//! Commands: requests flowing into the engine.
//!
//! A command travels inside an `Envelope`, which supplies its sequence number
//! and time, so commands carry no timestamp of their own.
//!
//! Parsing is strict: unknown fields are rejected, so neither the Rust types
//! nor the wire can express a priced market order or a mistagged command.
//!
//! The types do not validate values. Out-of-range quantities, prices and
//! order ids, and reused order ids, are rejected by the gateway, which needs no
//! book state for that; a command that names an unknown order is rejected by
//! the engine.

use serde::{Deserialize, Serialize};

use crate::{OrderId, Price, Qty, Side};

/// A request to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    /// Submit a new order.
    Place {
        /// Identifier of the new order.
        order_id: OrderId,
        /// Buy or sell.
        side: Side,
        /// How the order is priced and how long it lives.
        order_type: OrderType,
        /// Quantity ordered.
        qty: Qty,
    },
    /// Remove a resting order from the book.
    Cancel {
        /// The order to cancel.
        order_id: OrderId,
    },
    /// Change the price and quantity of a resting order.
    ///
    /// Gives the full new price and quantity, not a delta. A price change or
    /// a quantity increase loses time priority; a pure quantity decrease
    /// keeps it. A new price that trades on arrival makes the replace
    /// marketable: it trades like a new order, and a post-only order is
    /// refused instead.
    Replace {
        /// The order to replace.
        order_id: OrderId,
        /// Price after the change.
        price: Price,
        /// Open quantity after the change.
        qty: Qty,
    },
}

/// How an order is priced.
///
/// The limit price lives inside `Limit`, so a market order with a price, or a
/// limit order without one, cannot be expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum OrderType {
    /// Trades at its price or better.
    Limit {
        /// Worst acceptable price.
        price: Price,
        /// How the order trades on arrival and how long it rests.
        time_in_force: TimeInForce,
    },
    /// Trades against the best available prices; any remainder is canceled.
    ///
    /// A market order that trades at all is accepted; only an empty opposite
    /// side rejects it.
    ///
    /// An empty struct variant rather than a unit variant: serde ignores
    /// extra fields on unit variants even when unknown fields are denied.
    Market {},
}

/// How a limit order trades on arrival and how long it rests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum TimeInForce {
    /// Good till canceled: rests until filled or canceled.
    Gtc,
    /// Immediate or cancel: trades what it can immediately; the remainder is
    /// canceled.
    Ioc,
    /// Rests like `Gtc`, but is rejected if it would trade on arrival.
    PostOnly,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn error_of<T: serde::de::DeserializeOwned + std::fmt::Debug>(wire: Value) -> String {
        serde_json::from_value::<T>(wire).unwrap_err().to_string()
    }

    fn assert_wire(command: Command, expected: Value) {
        assert_eq!(serde_json::to_value(command).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<Command>(expected).unwrap(),
            command
        );
    }

    #[test]
    fn place_has_a_fixed_wire_form() {
        let command = Command::Place {
            order_id: 7,
            side: Side::Buy,
            order_type: OrderType::Limit {
                price: 100_250,
                time_in_force: TimeInForce::PostOnly,
            },
            qty: 3,
        };
        assert_wire(
            command,
            json!({
                "type": "place",
                "order_id": 7,
                "side": "buy",
                "order_type": { "type": "limit", "price": 100_250, "time_in_force": "post_only" },
                "qty": 3,
            }),
        );
    }

    #[test]
    fn cancel_and_replace_have_fixed_wire_forms() {
        assert_wire(
            Command::Cancel { order_id: 7 },
            json!({ "type": "cancel", "order_id": 7 }),
        );
        assert_wire(
            Command::Replace {
                order_id: 7,
                price: 100_300,
                qty: 2,
            },
            json!({ "type": "replace", "order_id": 7, "price": 100_300, "qty": 2 }),
        );
    }

    #[test]
    fn market_and_time_in_force_have_fixed_wire_forms() {
        let market = json!({ "type": "market" });
        assert_eq!(serde_json::to_value(OrderType::Market {}).unwrap(), market);
        assert_eq!(
            serde_json::from_value::<OrderType>(market).unwrap(),
            OrderType::Market {}
        );
        for (tif, wire) in [
            (TimeInForce::Gtc, "gtc"),
            (TimeInForce::Ioc, "ioc"),
            (TimeInForce::PostOnly, "post_only"),
        ] {
            assert_eq!(serde_json::to_value(tif).unwrap(), json!(wire));
            assert_eq!(
                serde_json::from_value::<TimeInForce>(json!(wire)).unwrap(),
                tif
            );
        }
    }

    #[test]
    fn limit_without_price_is_rejected() {
        let wire = json!({ "type": "limit", "time_in_force": "gtc" });
        assert!(error_of::<OrderType>(wire).contains("missing field `price`"));
    }

    #[test]
    fn unknown_time_in_force_is_rejected() {
        let wire = json!({ "type": "limit", "price": 1, "time_in_force": "fok" });
        assert!(error_of::<OrderType>(wire).contains("unknown variant `fok`"));
    }

    #[test]
    fn unknown_command_tag_is_rejected() {
        let wire = json!({ "type": "modify", "order_id": 7 });
        assert!(error_of::<Command>(wire).contains("unknown variant `modify`"));
    }

    #[test]
    fn market_with_a_price_is_rejected() {
        let wire = json!({ "type": "market", "price": 100 });
        assert!(error_of::<OrderType>(wire).contains("unknown field `price`"));
    }

    #[test]
    fn cancel_with_extra_fields_is_rejected() {
        let wire = json!({ "type": "cancel", "order_id": 7, "price": 100, "qty": 1 });
        assert!(error_of::<Command>(wire).contains("unknown field `price`"));
    }
}
