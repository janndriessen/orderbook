//! Market-data feed: aggregated book state and the event log, sent to clients.
//!
//! A client receives one `Snapshot`, then an `Update` roughly every 50 ms.
//! Each message's `seq` is exactly one greater than the previous one; on a gap
//! the client discards its state and reconnects, which starts with a fresh
//! snapshot.

use serde::{Deserialize, Serialize};

use crate::{Envelope, Event, Price, Qty, Seq};

/// Static description of the market, needed to display ticks and lots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct MarketInfo {
    /// Market symbol, e.g. `BTC-USD`.
    pub symbol: String,
    /// Decimal places of one tick: price = ticks / 10^price_decimals.
    pub price_decimals: u8,
    /// Decimal places of one lot: quantity = lots / 10^qty_decimals.
    pub qty_decimals: u8,
}

/// Total resting quantity at one price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Level {
    /// Price of the level.
    pub price: Price,
    /// Total quantity at the level; in an update, zero means the level is gone.
    pub qty: Qty,
}

/// Message from the server to a feed client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub enum FeedMessage {
    /// Full book state; replaces anything the client held before.
    Snapshot {
        /// Position within the feed; the next update has `seq + 1`.
        seq: Seq,
        /// Market the book belongs to.
        market: MarketInfo,
        /// Bid levels, best (highest) first.
        bids: Vec<Level>,
        /// Ask levels, best (lowest) first.
        asks: Vec<Level>,
        /// Most recent engine events, oldest first, to seed the flow log.
        events: Vec<Envelope<Event>>,
    },
    /// Changes since the previous message.
    Update {
        /// Position within the feed.
        seq: Seq,
        /// Bid levels that changed.
        bids: Vec<Level>,
        /// Ask levels that changed.
        asks: Vec<Level>,
        /// Engine events since the previous message, oldest first.
        events: Vec<Envelope<Event>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_round_trips() {
        let msg = FeedMessage::Update {
            seq: 42,
            bids: vec![Level { price: 99, qty: 0 }],
            asks: vec![],
            events: vec![Envelope {
                seq: 1_000,
                ts: 1_700_000_000_000_000,
                body: Event::Canceled {
                    order_id: 5,
                    leaves_qty: 2,
                },
            }],
        };
        let expected = json!({
            "type": "update",
            "seq": 42,
            "bids": [{ "price": 99, "qty": 0 }],
            "asks": [],
            "events": [{
                "seq": 1_000,
                "ts": 1_700_000_000_000_000_u64,
                "body": { "type": "canceled", "order_id": 5, "leaves_qty": 2 },
            }],
        });
        assert_eq!(serde_json::to_value(&msg).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<FeedMessage>(expected).unwrap(),
            msg
        );
    }
}
