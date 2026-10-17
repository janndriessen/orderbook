//! Message format shared by every component of the order book service.
//!
//! External sources never reach the engine directly: an adapter translates
//! them into these types first.
//!
//! Prices and quantities are integers (ticks and lots) so that no
//! floating-point rounding can creep into matching. Integer values stay below
//! 2^53 so that JavaScript clients represent them exactly.
//!
//! TypeScript definitions for the web client are generated from these types
//! into `web/src/protocol/` whenever the crate's tests run.

mod event;
mod feed;

pub use event::Event;
pub use feed::{FeedMessage, Level, MarketInfo};

use serde::{Deserialize, Serialize};

/// Unique order identifier, assigned by the order's source.
pub type OrderId = u64;

/// Position of a message within its stream; strictly increasing.
///
/// Gap-free on the stream that assigns it. A consumer that forwards only part
/// of a stream keeps the original numbers, so gaps there are expected.
pub type Seq = u64;

/// Price in ticks. Signed so that spreads and price differences need no casts.
pub type Price = i64;

/// Quantity in lots.
pub type Qty = u64;

/// Venue time in microseconds since the Unix epoch.
///
/// Microseconds rather than nanoseconds: current nanosecond timestamps exceed
/// 2^53 and would lose precision as JavaScript numbers.
pub type Ts = u64;

/// Side of an order or trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum Side {
    /// Bid: buys at or below its price.
    Buy,
    /// Ask: sells at or above its price.
    Sell,
}

/// Wraps every message on a stream with its sequence number and venue time.
///
/// `seq` lets the receiver detect gaps and reordering. `ts` is venue time, taken
/// from the source (e.g. the original Hyperliquid block time during replay),
/// never from the local clock, so a replay produces identical output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Envelope<T> {
    /// Position within the stream.
    pub seq: Seq,
    /// Venue time of the message.
    pub ts: Ts,
    /// The message itself.
    pub body: T,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn side_serializes_lowercase() {
        assert_eq!(serde_json::to_value(Side::Buy).unwrap(), json!("buy"));
        assert_eq!(serde_json::to_value(Side::Sell).unwrap(), json!("sell"));
    }

    #[test]
    fn envelope_round_trips() {
        let envelope = Envelope {
            seq: 1,
            ts: 1_700_000_000_000_000,
            body: Side::Buy,
        };
        let expected = json!({ "seq": 1, "ts": 1_700_000_000_000_000_u64, "body": "buy" });
        assert_eq!(serde_json::to_value(&envelope).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<Envelope<Side>>(expected).unwrap(),
            envelope
        );
    }
}
