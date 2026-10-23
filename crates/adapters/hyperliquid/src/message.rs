//! WebSocket messages (`l2Book`, `bbo` and `trades` channels) and their conversion
//! into integer form.
//!
//! The wire types mirror the JSON and keep numbers as the strings the source
//! sends. The `users` field of a trade carries wallet addresses and is
//! deliberately not declared, so it never appears in a parsed message. When
//! `data` comes before `channel`, the payload is held only while parsing.

use protocol::{MarketInfo, Price, Qty, Ts};

use serde::Deserialize;

use crate::decimal::{DecimalError, MAX_VALUE, parse_price, parse_qty};

/// The Hyperliquid coin the BTC-USD market is built from.
const COIN: &str = "BTC";

/// One message from the WebSocket, tagged by its `channel`.
///
/// A message on any other channel (subscription acknowledgements, `pong`,
/// `error`) fails to parse; a caller that needs to tell those apart from
/// malformed data reads `channel` first.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "channel", content = "data", rename_all = "camelCase")]
pub enum Message {
    /// Full snapshot of the top levels of the book.
    L2Book(WireBook),
    /// Best bid and offer, sent whenever either changes.
    Bbo(WireBbo),
    /// Trades since the last message.
    Trades(Vec<WireTrade>),
}

/// `bbo` payload as sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireBbo {
    /// Market name, e.g. `BTC`.
    pub coin: String,
    /// Time in milliseconds.
    pub time: u64,
    /// Best bid and best ask, in that order; `null` for an empty side.
    pub bbo: [Option<WireLevel>; 2],
}

/// `l2Book` payload as sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireBook {
    /// Market name, e.g. `BTC`.
    pub coin: String,
    /// Snapshot time in milliseconds.
    pub time: u64,
    /// Bids (best first) and asks (best first), in that order.
    pub levels: [Vec<WireLevel>; 2],
}

/// One price level as sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireLevel {
    /// Price as a decimal string.
    pub px: String,
    /// Total size as a decimal string.
    pub sz: String,
    /// Number of orders at the level.
    pub n: u64,
}

/// One trade as sent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireTrade {
    /// Market name, e.g. `BTC`.
    pub coin: String,
    /// Side marker.
    pub side: TradeSide,
    /// Price as a decimal string.
    pub px: String,
    /// Size as a decimal string.
    pub sz: String,
    /// Trade time in milliseconds.
    pub time: u64,
    /// Trade id.
    pub tid: u64,
}

/// The `side` marker of a trade, exactly as sent: `B` or `A`. The source's
/// documentation does not define it; on recorded data `B` trades print at the
/// ask, so it was observed to mark the buyer as the taker, and `A` the seller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum TradeSide {
    /// Sent as `B`.
    B,
    /// Sent as `A`.
    A,
}

/// Why a wire value could not be converted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConvertError {
    /// The message is for a coin other than `BTC`.
    #[error("unexpected coin {0}")]
    Coin(String),
    /// The `px` of a level or trade is not a valid price.
    #[error("invalid px: {0}")]
    Px(#[source] DecimalError),
    /// The `sz` of a level or trade is not a valid size.
    #[error("invalid sz: {0}")]
    Sz(#[source] DecimalError),
    /// The `time` in milliseconds does not fit in microseconds below 2^53, the
    /// protocol's bound for timestamps.
    #[error("time {0} ms is too large for microseconds")]
    TimeOverflow(u64),
    /// The best bid is at or above the best ask, which a resting book never
    /// shows; the message is a transient or corrupt view and is not usable.
    #[error("locked or crossed best bid and offer")]
    LockedOrCrossed,
}

/// A level in ticks and lots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookLevel {
    /// Price in ticks.
    pub price: Price,
    /// Total size in lots.
    pub qty: Qty,
    /// Number of orders at the level.
    pub orders: u64,
}

/// A book snapshot in integer form. Level order and shape are taken from the
/// wire unchecked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSnapshot {
    /// Snapshot time in microseconds.
    pub ts: Ts,
    /// Bid levels as sent.
    pub bids: Vec<BookLevel>,
    /// Ask levels as sent.
    pub asks: Vec<BookLevel>,
}

/// A trade in integer form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trade {
    /// Trade time in microseconds.
    pub ts: Ts,
    /// Price in ticks.
    pub price: Price,
    /// Size in lots.
    pub qty: Qty,
    /// Side marker as sent.
    pub side: TradeSide,
    /// Trade id.
    pub tid: u64,
}

fn check_coin(coin: &str) -> Result<(), ConvertError> {
    if coin == COIN {
        Ok(())
    } else {
        Err(ConvertError::Coin(coin.to_string()))
    }
}

fn micros(millis: u64) -> Result<Ts, ConvertError> {
    millis
        .checked_mul(1000)
        .filter(|micros| *micros <= MAX_VALUE)
        .ok_or(ConvertError::TimeOverflow(millis))
}

fn level(wire: &WireLevel, market: &MarketInfo) -> Result<BookLevel, ConvertError> {
    Ok(BookLevel {
        price: parse_price(&wire.px, market).map_err(ConvertError::Px)?,
        qty: parse_qty(&wire.sz, market).map_err(ConvertError::Sz)?,
        orders: wire.n,
    })
}

/// Converts one snapshot's sides, failing on the first bad value.
fn snapshot(
    coin: &str,
    time: u64,
    bids: &[WireLevel],
    asks: &[WireLevel],
    market: &MarketInfo,
) -> Result<BookSnapshot, ConvertError> {
    check_coin(coin)?;
    let side = |levels: &[WireLevel]| -> Result<Vec<BookLevel>, ConvertError> {
        levels.iter().map(|l| level(l, market)).collect()
    };
    Ok(BookSnapshot {
        ts: micros(time)?,
        bids: side(bids)?,
        asks: side(asks)?,
    })
}

impl WireBook {
    /// Converts to ticks and lots, failing on the first bad value.
    ///
    /// # Errors
    ///
    /// Returns a [`ConvertError`] naming the field that failed.
    pub fn to_snapshot(&self, market: &MarketInfo) -> Result<BookSnapshot, ConvertError> {
        snapshot(
            &self.coin,
            self.time,
            &self.levels[0],
            &self.levels[1],
            market,
        )
    }
}

impl WireBbo {
    /// Converts to a one-level-per-side snapshot; an absent side is empty.
    ///
    /// An empty side says nothing about where its window ends, so replaying
    /// the snapshot leaves that side alone; stale levels there can cross the
    /// other side's new level and make a maker be rejected
    /// (`rejected_makers`).
    ///
    /// # Errors
    ///
    /// Returns a [`ConvertError`] naming the field that failed, or
    /// [`ConvertError::LockedOrCrossed`] when both sides are present and the
    /// bid is not below the ask.
    pub fn to_snapshot(&self, market: &MarketInfo) -> Result<BookSnapshot, ConvertError> {
        let [bid, ask] = &self.bbo;
        let snapshot = snapshot(
            &self.coin,
            self.time,
            bid.as_slice(),
            ask.as_slice(),
            market,
        )?;
        match (snapshot.bids.first(), snapshot.asks.first()) {
            (Some(bid), Some(ask)) if bid.price >= ask.price => Err(ConvertError::LockedOrCrossed),
            _ => Ok(snapshot),
        }
    }
}

impl WireTrade {
    /// Converts to ticks and lots.
    ///
    /// # Errors
    ///
    /// Returns a [`ConvertError`] naming the field that failed.
    pub fn to_trade(&self, market: &MarketInfo) -> Result<Trade, ConvertError> {
        check_coin(&self.coin)?;
        Ok(Trade {
            ts: micros(self.time)?,
            price: parse_price(&self.px, market).map_err(ConvertError::Px)?,
            qty: parse_qty(&self.sz, market).map_err(ConvertError::Sz)?,
            side: self.side,
            tid: self.tid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market() -> MarketInfo {
        MarketInfo {
            symbol: "BTC-USD".to_string(),
            price_decimals: 0,
            qty_decimals: 5,
        }
    }

    const BOOK: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1792599271750,"levels":[
        [{"px":"84579.0","sz":"3.07937","n":22},{"px":"84578.0","sz":"0.12415","n":1},{"px":"84577.0","sz":"0.003","n":2}],
        [{"px":"84580.0","sz":"12.83161","n":51},{"px":"84581.0","sz":"0.33641","n":2},{"px":"84582.0","sz":"0.71902","n":1}]]}}"#;

    const TRADES: &str = r#"{"channel":"trades","data":[
        {"coin":"BTC","side":"B","px":"84580.0","sz":"0.01500","hash":"0x0000000000000000000000000000000000000000000000000000000000000001","time":1792599272000,"tid":1001,"users":["0x0000000000000000000000000000000000000001","0x0000000000000000000000000000000000000002"]},
        {"coin":"BTC","side":"A","px":"84579.0","sz":"0.2","hash":"0x0000000000000000000000000000000000000000000000000000000000000002","time":1792599272001,"tid":1002,"users":["0x0000000000000000000000000000000000000003","0x0000000000000000000000000000000000000004"]}]}"#;

    fn parse(text: &str) -> Message {
        serde_json::from_str(text).expect("fixture parses")
    }

    fn book_with(px: &str, sz: &str, time: u64) -> WireBook {
        WireBook {
            coin: "BTC".to_string(),
            time,
            levels: [
                vec![WireLevel {
                    px: px.to_string(),
                    sz: sz.to_string(),
                    n: 1,
                }],
                vec![],
            ],
        }
    }

    fn wire(px: &str, sz: &str) -> Option<WireLevel> {
        Some(WireLevel {
            px: px.to_string(),
            sz: sz.to_string(),
            n: 1,
        })
    }

    fn bbo_with(bid: Option<WireLevel>, ask: Option<WireLevel>, time: u64) -> WireBbo {
        WireBbo {
            coin: "BTC".to_string(),
            time,
            bbo: [bid, ask],
        }
    }

    fn trade(px: &str, sz: &str, time: u64) -> WireTrade {
        WireTrade {
            coin: "BTC".to_string(),
            side: TradeSide::B,
            px: px.to_string(),
            sz: sz.to_string(),
            time,
            tid: 1,
        }
    }

    fn lvl(price: Price, qty: Qty, orders: u64) -> BookLevel {
        BookLevel { price, qty, orders }
    }

    #[test]
    fn converts_a_book_snapshot() {
        let Message::L2Book(book) = parse(BOOK) else {
            panic!("expected l2Book");
        };
        assert_eq!(
            book.to_snapshot(&market()),
            Ok(BookSnapshot {
                ts: 1_792_599_271_750_000,
                bids: vec![
                    lvl(84579, 307_937, 22),
                    lvl(84578, 12_415, 1),
                    lvl(84577, 300, 2)
                ],
                asks: vec![
                    lvl(84580, 1_283_161, 51),
                    lvl(84581, 33_641, 2),
                    lvl(84582, 71_902, 1)
                ],
            })
        );
    }

    #[test]
    fn converts_a_best_bid_and_offer() {
        let text = r#"{"channel":"bbo","data":{"coin":"BTC","time":1792599271750,"bbo":[
            {"px":"84579.0","sz":"3.07937","n":22},{"px":"84580.0","sz":"12.83161","n":51}]}}"#;
        let Message::Bbo(bbo) = parse(text) else {
            panic!("expected bbo");
        };
        assert_eq!(
            bbo.to_snapshot(&market()),
            Ok(BookSnapshot {
                ts: 1_792_599_271_750_000,
                bids: vec![lvl(84579, 307_937, 22)],
                asks: vec![lvl(84580, 1_283_161, 51)],
            })
        );
    }

    #[test]
    fn a_null_side_of_a_best_bid_and_offer_is_empty() {
        let text = r#"{"channel":"bbo","data":{"coin":"BTC","time":5,"bbo":[null,{"px":"7","sz":"1","n":2}]}}"#;
        let Message::Bbo(bbo) = parse(text) else {
            panic!("expected bbo");
        };
        assert_eq!(
            bbo.to_snapshot(&market()),
            Ok(BookSnapshot {
                ts: 5_000,
                bids: vec![],
                asks: vec![lvl(7, 100_000, 2)],
            })
        );
        let empty = bbo_with(None, None, 1).to_snapshot(&market());
        assert_eq!(empty.map(|s| (s.bids.len(), s.asks.len())), Ok((0, 0)));
    }

    #[test]
    fn a_best_bid_and_offer_checks_coin_time_and_numbers() {
        let mut eth = bbo_with(wire("1", "1"), None, 1);
        eth.coin = "ETH".to_string();
        assert_eq!(
            eth.to_snapshot(&market()),
            Err(ConvertError::Coin("ETH".into()))
        );
        assert_eq!(
            bbo_with(wire("1", "1"), None, 9_007_199_254_741).to_snapshot(&market()),
            Err(ConvertError::TimeOverflow(9_007_199_254_741))
        );
        assert_eq!(
            bbo_with(wire("1.5", "1"), None, 1).to_snapshot(&market()),
            Err(ConvertError::Px(DecimalError::TooManyDecimals(0)))
        );
        assert_eq!(
            bbo_with(None, wire("2", "1e3"), 1).to_snapshot(&market()),
            Err(ConvertError::Sz(DecimalError::Invalid))
        );
    }

    #[test]
    fn a_locked_or_crossed_best_bid_and_offer_is_its_own_error() {
        for (bid, ask) in [("5", "5"), ("6", "5")] {
            assert_eq!(
                bbo_with(wire(bid, "1"), wire(ask, "1"), 1).to_snapshot(&market()),
                Err(ConvertError::LockedOrCrossed)
            );
        }
        assert!(
            bbo_with(wire("5", "1"), wire("6", "1"), 1)
                .to_snapshot(&market())
                .is_ok()
        );
    }

    #[test]
    fn converts_trades() {
        let Message::Trades(trades) = parse(TRADES) else {
            panic!("expected trades");
        };
        let converted: Result<Vec<_>, _> = trades.iter().map(|t| t.to_trade(&market())).collect();
        assert_eq!(
            converted,
            Ok(vec![
                Trade {
                    ts: 1_792_599_272_000_000,
                    price: 84580,
                    qty: 1_500,
                    side: TradeSide::B,
                    tid: 1001
                },
                Trade {
                    ts: 1_792_599_272_001_000,
                    price: 84579,
                    qty: 20_000,
                    side: TradeSide::A,
                    tid: 1002
                },
            ])
        );
    }

    #[test]
    fn unknown_channels_are_errors() {
        for text in [
            r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#,
            r#"{"channel":"error","data":"Invalid subscription"}"#,
            r#"{"channel":"pong"}"#,
        ] {
            assert!(serde_json::from_str::<Message>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn data_may_come_before_channel() {
        let text = r#"{"data":[{"coin":"BTC","side":"A","px":"1","sz":"1","time":1,"tid":7}],"channel":"trades"}"#;
        assert!(matches!(parse(text), Message::Trades(t) if t.len() == 1));
    }

    #[test]
    fn trade_addresses_are_not_kept() {
        let payload = TRADES
            .strip_prefix(r#"{"channel":"trades","data":"#)
            .and_then(|text| text.strip_suffix('}'))
            .expect("fixture has the expected envelope");
        let data_first = format!(r#"{{"data":{payload},"channel":"trades"}}"#);
        for text in [TRADES, data_first.as_str()] {
            let parsed = format!("{:?}", parse(text));
            assert!(!parsed.contains("0x"), "{parsed}");
        }
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let text = r#"{"channel":"trades","extra":1,"data":[{"coin":"BTC","side":"A","px":"1","sz":"1","time":1,"tid":7,"new":true}]}"#;
        assert!(matches!(parse(text), Message::Trades(t) if t.len() == 1));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(serde_json::from_str::<Message>(r#"{"channel":"l2Book","data":{"#).is_err());
        assert!(serde_json::from_str::<Message>("not json").is_err());
    }

    #[test]
    fn rejects_known_channels_with_the_wrong_shape() {
        for text in [
            r#"{"channel":"trades","data":{"coin":"BTC"}}"#,
            r#"{"channel":"trades","data":[{"coin":"BTC","side":"X","px":"1","sz":"1","time":1,"tid":1}]}"#,
            r#"{"channel":"l2Book","data":{"coin":"BTC","time":1,"levels":[[]]}}"#,
        ] {
            assert!(serde_json::from_str::<Message>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_bad_price_in_one_level_fails_the_snapshot() {
        let mut book = book_with("84579.0", "1", 1);
        book.levels[1].push(WireLevel {
            px: "84580.5".to_string(),
            sz: "1".to_string(),
            n: 1,
        });
        assert_eq!(
            book.to_snapshot(&market()),
            Err(ConvertError::Px(DecimalError::TooManyDecimals(0)))
        );
    }

    #[test]
    fn a_bad_size_in_a_level_names_the_field() {
        let book = book_with("84579.0", "1e3", 1);
        assert_eq!(
            book.to_snapshot(&market()),
            Err(ConvertError::Sz(DecimalError::Invalid))
        );
    }

    #[test]
    fn a_bad_size_in_a_trade_names_the_field() {
        let bad = trade("84580.0", "-1", 1);
        assert_eq!(
            bad.to_trade(&market()),
            Err(ConvertError::Sz(DecimalError::Invalid))
        );
    }

    #[test]
    fn a_bad_price_in_a_trade_names_the_field() {
        assert_eq!(
            trade("0.5", "1", 1).to_trade(&market()),
            Err(ConvertError::Px(DecimalError::TooManyDecimals(0)))
        );
    }

    #[test]
    fn other_coins_are_rejected() {
        let mut book = book_with("1", "1", 1);
        book.coin = "ETH".to_string();
        assert_eq!(
            book.to_snapshot(&market()),
            Err(ConvertError::Coin("ETH".into()))
        );
        let mut eth = trade("1", "1", 1);
        eth.coin = "ETH".to_string();
        assert_eq!(
            eth.to_trade(&market()),
            Err(ConvertError::Coin("ETH".into()))
        );
    }

    #[test]
    fn time_is_accepted_up_to_the_protocol_bound() {
        // 9_007_199_254_740 ms is the largest whole millisecond below 2^53 µs.
        assert_eq!(
            book_with("1", "1", 9_007_199_254_740)
                .to_snapshot(&market())
                .map(|s| s.ts),
            Ok(9_007_199_254_740_000)
        );
        assert_eq!(
            trade("1", "1", 9_007_199_254_740)
                .to_trade(&market())
                .map(|t| t.ts),
            Ok(9_007_199_254_740_000)
        );
    }

    #[test]
    fn time_beyond_the_protocol_bound_is_rejected() {
        for time in [
            9_007_199_254_741,
            u64::MAX / 1000,
            u64::MAX / 1000 + 1,
            u64::MAX,
        ] {
            assert_eq!(
                book_with("1", "1", time).to_snapshot(&market()),
                Err(ConvertError::TimeOverflow(time))
            );
            assert_eq!(
                trade("1", "1", time).to_trade(&market()),
                Err(ConvertError::TimeOverflow(time))
            );
        }
    }

    #[test]
    fn known_channels_need_their_data() {
        for text in [
            r#"{"channel":"l2Book"}"#,
            r#"{"channel":"trades"}"#,
            r#"{"channel":"trades","data":null}"#,
            r#"{"data":[]}"#,
        ] {
            assert!(serde_json::from_str::<Message>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn duplicate_envelope_keys_are_rejected() {
        for text in [
            r#"{"channel":"trades","channel":"trades","data":[]}"#,
            r#"{"channel":"trades","data":[],"data":[]}"#,
            r#"{"data":[],"channel":"trades","data":[]}"#,
        ] {
            assert!(serde_json::from_str::<Message>(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_array_form_is_accepted() {
        assert_eq!(parse(r#"["trades",[]]"#), Message::Trades(vec![]));
    }

    #[test]
    fn duplicate_fields_and_scalars_are_rejected() {
        for text in [
            r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"1","px":"2","sz":"1","time":1,"tid":1}]}"#,
            r#""trades""#,
            r#"42"#,
            r#"{"channel":"l2Book","data":{"coin":"BTC","time":1,"levels":[[],[],[]]}}"#,
            r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":1,"sz":"1","time":1,"tid":1}]}"#,
            r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"1","sz":"1","time":-1,"tid":1}]}"#,
            r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"1","sz":"1","time":1}]}"#,
        ] {
            assert!(serde_json::from_str::<Message>(text).is_err(), "{text}");
        }
    }
}
