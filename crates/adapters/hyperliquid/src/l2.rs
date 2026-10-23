//! The `l2` channel: the book feed the source's own web app subscribes to.
//!
//! It is not in the source's API documentation. After subscribing, the first
//! message holds a full book of up to 20 levels per side, and every later one a
//! change to it, compressed as base64 of a raw deflate stream of JSON. A change
//! names the levels that are gone by their index in the book before the
//! change, and the levels that are new or resized by price. Replaying recorded
//! changes this way gave exactly the book of a fresh subscription at the same
//! push. A subscription is for one price grouping, but no message says which,
//! so a caller keeps one decoder per subscription.

use std::io::Read;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::DeflateDecoder;
use protocol::{MarketInfo, Price, Side};
use serde::Deserialize;

use crate::decimal::{parse_price, parse_qty};
use crate::message::{
    BookLevel, BookSnapshot, ConvertError, WireBook, check_coin, check_not_crossed, micros,
};

/// Most bytes one change may inflate to. Recorded changes inflate to under
/// 3 KiB; the bound keeps a corrupt message from taking unbounded memory.
const MAX_CHANGE_BYTES: u64 = 1 << 20;

/// Most levels a side may hold. The source sends up to 20; a deeper side
/// means the stream is no longer the one this decoder understands.
const MAX_DEPTH: usize = 20;

/// `l2` payload as sent: exactly one of a full book or a compressed change.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WireL2 {
    /// The full book, in the shape of an `l2Book` payload.
    pub s: Option<WireBook>,
    /// A change to the book: base64 of a raw deflate stream of JSON.
    pub c: Option<String>,
}

/// A change after decompression.
#[derive(Deserialize)]
struct WireChange {
    /// Market name, e.g. `BTC`.
    c: String,
    /// Time in milliseconds.
    t: u64,
    /// New or resized levels of the bids and of the asks.
    l: [Vec<WireChangedLevel>; 2],
    /// Indices of the bid and of the ask levels that are gone, counted in
    /// the book before the change.
    r: [Vec<usize>; 2],
}

/// One new or resized level of a change.
#[derive(Deserialize)]
struct WireChangedLevel {
    /// Price as a decimal string.
    p: String,
    /// Total size as a decimal string.
    s: String,
}

/// Why an `l2` message could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum L2Error {
    /// The payload holds neither a book nor a change, or both.
    #[error("an l2 message must hold exactly one of a book or a change")]
    Shape,
    /// A change arrived with no book to apply it to: before the first book,
    /// or after a message that failed.
    #[error("an l2 change arrived without a book to apply it to")]
    NoBook,
    /// The change is not valid base64.
    #[error("l2 change is not base64: {0}")]
    Base64(#[source] base64::DecodeError),
    /// The change is not a raw deflate stream.
    #[error("l2 change does not inflate: {0}")]
    Inflate(#[source] std::io::Error),
    /// The change inflates to more than 1 MiB.
    #[error("l2 change inflates to more than {MAX_CHANGE_BYTES} bytes")]
    TooLarge,
    /// The inflated change is not the expected JSON.
    #[error("l2 change is not valid: {0}")]
    Json(#[source] serde_json::Error),
    /// A removed index is past the end of its side.
    #[error("l2 change removes index {index} of a side with {len} levels")]
    Removed {
        /// The index as sent.
        index: usize,
        /// Levels on that side before the change.
        len: usize,
    },
    /// A change removes the same index twice on one side.
    #[error("l2 change removes index {index} twice")]
    RemovedTwice {
        /// The repeated index.
        index: usize,
    },
    /// A change is older than the book it applies to.
    #[error("l2 change is older than its book")]
    Backwards,
    /// A change sets the same price twice on one side.
    #[error("l2 change sets price {price} twice on one side")]
    RepeatedPrice {
        /// The repeated price in ticks.
        price: Price,
    },
    /// A side of the book is not in strict best-first order, so indices into
    /// it cannot name the levels the source means.
    #[error("l2 book side is not in strict best-first order")]
    Unsorted,
    /// A side holds more than 20 levels.
    #[error("l2 book side holds {len} levels, more than 20")]
    TooDeep {
        /// Levels on that side.
        len: usize,
    },
    /// A level has a size of zero. The source never sent one; it might
    /// stand for a removal this decoder does not know about.
    #[error("l2 level at price {price} has size zero")]
    ZeroSize {
        /// The level's price in ticks.
        price: Price,
    },
    /// A price, size, time or coin cannot be converted, or the book's best
    /// bid is at or above its best ask ([`ConvertError::LockedOrCrossed`]).
    #[error(transparent)]
    Convert(#[from] ConvertError),
}

/// The book of one `l2` subscription, kept up to date message by message.
#[derive(Debug, Default)]
pub struct L2Decoder {
    book: Option<BookSnapshot>,
}

impl L2Decoder {
    /// A decoder that has seen no book yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one message and returns the book as it now stands.
    ///
    /// A book message replaces the book; a change applies to the book before
    /// it. Every resulting book must be strictly best first, hold at most 20
    /// levels per side and no level of size zero, and have its best bid below
    /// its best ask. When a message fails, for any reason, the decoder drops
    /// its book: a failed message may be a missed change, after which the
    /// indices of every later change would name the wrong levels. Changes
    /// then fail with [`L2Error::NoBook`] until a book arrives again.
    ///
    /// # Errors
    ///
    /// Returns an [`L2Error`] saying what was wrong with the message.
    pub fn apply(&mut self, wire: &WireL2, market: &MarketInfo) -> Result<&BookSnapshot, L2Error> {
        let previous = self.book.take();
        let next = match (&wire.s, &wire.c) {
            (Some(book), None) => book.to_snapshot(market)?,
            (None, Some(change)) => {
                apply_change(&previous.ok_or(L2Error::NoBook)?, change, market)?
            }
            _ => return Err(L2Error::Shape),
        };
        check_book(&next)?;
        Ok(self.book.insert(next))
    }
}

/// The checks every book must pass, whether sent whole or built from changes.
fn check_book(book: &BookSnapshot) -> Result<(), L2Error> {
    for (levels, side) in [(&book.bids, Side::Buy), (&book.asks, Side::Sell)] {
        if !best_first(levels, side) {
            return Err(L2Error::Unsorted);
        }
        if levels.len() > MAX_DEPTH {
            return Err(L2Error::TooDeep { len: levels.len() });
        }
        if let Some(level) = levels.iter().find(|level| level.qty == 0) {
            return Err(L2Error::ZeroSize { price: level.price });
        }
    }
    Ok(check_not_crossed(book)?)
}

/// The order of a side's levels: bids by falling price, asks by rising price.
fn rank(side: Side, price: Price) -> Price {
    match side {
        Side::Buy => -price,
        Side::Sell => price,
    }
}

fn best_first(levels: &[BookLevel], side: Side) -> bool {
    levels
        .windows(2)
        .all(|pair| rank(side, pair[0].price) < rank(side, pair[1].price))
}

fn apply_change(
    book: &BookSnapshot,
    change: &str,
    market: &MarketInfo,
) -> Result<BookSnapshot, L2Error> {
    let compressed = STANDARD.decode(change).map_err(L2Error::Base64)?;
    let mut json = Vec::new();
    DeflateDecoder::new(compressed.as_slice())
        .take(MAX_CHANGE_BYTES + 1)
        .read_to_end(&mut json)
        .map_err(L2Error::Inflate)?;
    if json.len() as u64 > MAX_CHANGE_BYTES {
        return Err(L2Error::TooLarge);
    }
    let change: WireChange = serde_json::from_slice(&json).map_err(L2Error::Json)?;
    check_coin(&change.c)?;
    let ts = micros(change.t)?;
    if ts < book.ts {
        return Err(L2Error::Backwards);
    }
    let [bid_levels, ask_levels] = &change.l;
    let [bid_removed, ask_removed] = &change.r;
    Ok(BookSnapshot {
        ts,
        bids: apply_side(&book.bids, bid_removed, bid_levels, Side::Buy, market)?,
        asks: apply_side(&book.asks, ask_removed, ask_levels, Side::Sell, market)?,
    })
}

/// One side after a change: the removed indices dropped first, then each
/// changed level set at its price, keeping the side best first.
fn apply_side(
    levels: &[BookLevel],
    removed: &[usize],
    changed: &[WireChangedLevel],
    side: Side,
    market: &MarketInfo,
) -> Result<Vec<BookLevel>, L2Error> {
    let len = levels.len();
    let mut gone = vec![false; len];
    for &index in removed {
        match gone.get_mut(index) {
            Some(slot) if *slot => return Err(L2Error::RemovedTwice { index }),
            Some(slot) => *slot = true,
            None => return Err(L2Error::Removed { index, len }),
        }
    }
    let mut kept: Vec<BookLevel> = levels
        .iter()
        .zip(&gone)
        .filter(|&(_, &gone)| !gone)
        .map(|(level, _)| *level)
        .collect();
    let changed = changed
        .iter()
        .map(|wire| {
            Ok(BookLevel {
                price: parse_price(&wire.p, market).map_err(ConvertError::Px)?,
                qty: parse_qty(&wire.s, market).map_err(ConvertError::Sz)?,
                orders: 0,
            })
        })
        .collect::<Result<Vec<_>, ConvertError>>()?;
    let mut prices: Vec<Price> = changed.iter().map(|level| level.price).collect();
    prices.sort_unstable();
    if let Some(pair) = prices.windows(2).find(|pair| pair[0] == pair[1]) {
        return Err(L2Error::RepeatedPrice { price: pair[0] });
    }
    for level in changed {
        let key = rank(side, level.price);
        match kept.binary_search_by_key(&key, |l| rank(side, l.price)) {
            Ok(at) => kept[at] = level,
            Err(at) => kept.insert(at, level),
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::DeflateEncoder;

    use super::*;
    use crate::Message;

    fn market() -> MarketInfo {
        MarketInfo {
            symbol: "BTC-USD".to_string(),
            price_decimals: 0,
            qty_decimals: 5,
        }
    }

    fn compress(bytes: &[u8]) -> String {
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).expect("writing to a Vec succeeds");
        STANDARD.encode(encoder.finish().expect("finishing a Vec succeeds"))
    }

    fn change(json: &str) -> WireL2 {
        WireL2 {
            s: None,
            c: Some(compress(json.as_bytes())),
        }
    }

    /// Bids 100, 98, 96 and asks 101, 103, 105, one lot and one order each;
    /// the gaps leave room for new levels between them.
    fn book() -> WireL2 {
        let text = r#"{"coin":"BTC","time":1,"spread":"1.0","levels":[
            [{"px":"100.0","sz":"0.00001","n":1},{"px":"98.0","sz":"0.00001","n":1},{"px":"96.0","sz":"0.00001","n":1}],
            [{"px":"101.0","sz":"0.00001","n":1},{"px":"103.0","sz":"0.00001","n":1},{"px":"105.0","sz":"0.00001","n":1}]]}"#;
        WireL2 {
            s: Some(serde_json::from_str(text).expect("fixture parses")),
            c: None,
        }
    }

    fn lvl(price: Price, qty: u64, orders: u64) -> BookLevel {
        BookLevel { price, qty, orders }
    }

    fn started() -> L2Decoder {
        let mut decoder = L2Decoder::new();
        decoder.apply(&book(), &market()).expect("book applies");
        decoder
    }

    /// The result of one message applied after the book.
    fn after_book(wire: &WireL2) -> Result<BookSnapshot, L2Error> {
        started().apply(wire, &market()).cloned()
    }

    #[test]
    fn a_book_message_is_the_book() {
        let mut decoder = L2Decoder::new();
        let snapshot = decoder.apply(&book(), &market()).expect("book applies");
        assert_eq!(snapshot.ts, 1_000);
        assert_eq!(
            snapshot.bids,
            vec![lvl(100, 1, 1), lvl(98, 1, 1), lvl(96, 1, 1)]
        );
        assert_eq!(
            snapshot.asks,
            vec![lvl(101, 1, 1), lvl(103, 1, 1), lvl(105, 1, 1)]
        );
    }

    #[test]
    fn a_change_removes_by_index_then_sets_by_price() {
        let mut decoder = started();
        // Bids: index 1 (98) goes, 100 is resized, 95 is new at the far end.
        // Asks: index 0 (101) goes, 106 is new; 101 comes back in the same
        // change, which works only because removals apply first.
        let json = r#"{"c":"BTC","t":2,"s":"1.0",
            "l":[[{"p":"100.0","s":"0.00005"},{"p":"95.0","s":"0.00002"}],
                 [{"p":"106.0","s":"0.00003"},{"p":"101.0","s":"0.00007"}]],
            "r":[[1],[0]]}"#;
        let snapshot = decoder
            .apply(&change(json), &market())
            .expect("change applies");
        assert_eq!(snapshot.ts, 2_000);
        assert_eq!(
            snapshot.bids,
            vec![lvl(100, 5, 0), lvl(96, 1, 1), lvl(95, 2, 0)]
        );
        assert_eq!(
            snapshot.asks,
            vec![
                lvl(101, 7, 0),
                lvl(103, 1, 1),
                lvl(105, 1, 1),
                lvl(106, 3, 0)
            ]
        );
    }

    #[test]
    fn indices_count_in_the_book_before_the_change() {
        let mut decoder = started();
        // A new bid at 99 lands before index 2, but index 2 still names 96.
        let json = r#"{"c":"BTC","t":2,"l":[[{"p":"99.0","s":"0.00004"}],[]],"r":[[2],[]]}"#;
        let snapshot = decoder
            .apply(&change(json), &market())
            .expect("change applies");
        assert_eq!(
            snapshot.bids,
            vec![lvl(100, 1, 1), lvl(99, 4, 0), lvl(98, 1, 1)]
        );
    }

    #[test]
    fn a_change_before_any_book_is_an_error() {
        let json = r#"{"c":"BTC","t":2,"l":[[],[]],"r":[[],[]]}"#;
        let result = L2Decoder::new().apply(&change(json), &market()).cloned();
        assert!(matches!(result, Err(L2Error::NoBook)));
    }

    #[test]
    fn a_failed_change_drops_the_book_until_the_next_one() {
        let mut decoder = started();
        let bad = r#"{"c":"BTC","t":2,"l":[[],[]],"r":[[3],[]]}"#;
        assert!(matches!(
            decoder.apply(&change(bad), &market()),
            Err(L2Error::Removed { index: 3, len: 3 })
        ));
        let good = r#"{"c":"BTC","t":3,"l":[[],[]],"r":[[],[]]}"#;
        assert!(matches!(
            decoder.apply(&change(good), &market()),
            Err(L2Error::NoBook)
        ));
        decoder.apply(&book(), &market()).expect("book applies");
        let snapshot = decoder
            .apply(&change(good), &market())
            .expect("change applies");
        assert_eq!((snapshot.ts, snapshot.bids.len()), (3_000, 3));
    }

    #[test]
    fn a_failed_book_or_envelope_drops_the_book_too() {
        let unsorted = WireL2 {
            s: Some(
                serde_json::from_str(
                    r#"{"coin":"BTC","time":1,"levels":[[{"px":"1","sz":"1","n":1},{"px":"2","sz":"1","n":1}],[]]}"#,
                )
                .expect("fixture parses"),
            ),
            c: None,
        };
        let mut eth = book();
        if let Some(book) = eth.s.as_mut() {
            book.coin = "ETH".to_string();
        }
        let empty = WireL2 { s: None, c: None };
        let good = r#"{"c":"BTC","t":3,"l":[[],[]],"r":[[],[]]}"#;
        for bad in [unsorted, eth, empty] {
            let mut decoder = started();
            assert!(decoder.apply(&bad, &market()).is_err());
            assert!(matches!(
                decoder.apply(&change(good), &market()),
                Err(L2Error::NoBook)
            ));
        }
    }

    #[test]
    fn a_zero_size_is_rejected() {
        let json = r#"{"c":"BTC","t":2,"l":[[{"p":"99.0","s":"0"}],[]],"r":[[],[]]}"#;
        assert!(matches!(
            after_book(&change(json)),
            Err(L2Error::ZeroSize { price: 99 })
        ));
        let text = r#"{"coin":"BTC","time":1,"levels":[[{"px":"100.0","sz":"0.0","n":0}],[]]}"#;
        let wire = WireL2 {
            s: Some(serde_json::from_str(text).expect("fixture parses")),
            c: None,
        };
        let result = L2Decoder::new().apply(&wire, &market()).cloned();
        assert!(matches!(result, Err(L2Error::ZeroSize { price: 100 })));
    }

    #[test]
    fn a_change_may_set_a_price_once_per_side() {
        let json = r#"{"c":"BTC","t":2,"l":[[{"p":"99.0","s":"0.00001"},{"p":"99.0","s":"0.00009"}],[]],"r":[[],[]]}"#;
        assert!(matches!(
            after_book(&change(json)),
            Err(L2Error::RepeatedPrice { price: 99 })
        ));
        // The same price on both sides is a crossed book, not a repeat.
        let json = r#"{"c":"BTC","t":2,"l":[[{"p":"102.0","s":"0.00001"}],[{"p":"102.0","s":"0.00001"}]],"r":[[],[]]}"#;
        assert!(matches!(
            after_book(&change(json)),
            Err(L2Error::Convert(ConvertError::LockedOrCrossed))
        ));
    }

    #[test]
    fn a_side_holds_at_most_twenty_levels() {
        // The book has 3 bids; 17 more make 20, which is allowed.
        let levels = |count: i64| -> String {
            (0..count)
                .map(|i| format!(r#"{{"p":"{}.0","s":"0.00001"}}"#, 50 + i))
                .collect::<Vec<_>>()
                .join(",")
        };
        let twenty = format!(
            r#"{{"c":"BTC","t":2,"l":[[{}],[]],"r":[[],[]]}}"#,
            levels(17)
        );
        let snapshot = after_book(&change(&twenty)).expect("twenty levels apply");
        assert_eq!(snapshot.bids.len(), 20);
        let deeper = format!(
            r#"{{"c":"BTC","t":2,"l":[[{}],[]],"r":[[],[]]}}"#,
            levels(18)
        );
        assert!(matches!(
            after_book(&change(&deeper)),
            Err(L2Error::TooDeep { len: 21 })
        ));
    }

    #[test]
    fn a_locked_or_crossed_book_is_rejected() {
        for price in ["101.0", "102.0"] {
            let json = format!(
                r#"{{"c":"BTC","t":2,"l":[[{{"p":"{price}","s":"0.00001"}}],[]],"r":[[],[]]}}"#
            );
            assert!(
                matches!(
                    after_book(&change(&json)),
                    Err(L2Error::Convert(ConvertError::LockedOrCrossed))
                ),
                "{price}"
            );
        }
        let text = r#"{"coin":"BTC","time":1,"levels":[[{"px":"5","sz":"1","n":1}],[{"px":"5","sz":"1","n":1}]]}"#;
        let wire = WireL2 {
            s: Some(serde_json::from_str(text).expect("fixture parses")),
            c: None,
        };
        let result = L2Decoder::new().apply(&wire, &market()).cloned();
        assert!(matches!(
            result,
            Err(L2Error::Convert(ConvertError::LockedOrCrossed))
        ));
    }

    #[test]
    fn a_change_may_not_be_older_than_its_book() {
        let older = r#"{"c":"BTC","t":0,"l":[[],[]],"r":[[],[]]}"#;
        assert!(matches!(
            after_book(&change(older)),
            Err(L2Error::Backwards)
        ));
        let same = r#"{"c":"BTC","t":1,"l":[[],[]],"r":[[],[]]}"#;
        assert_eq!(after_book(&change(same)).map(|s| s.ts).ok(), Some(1_000));
    }

    #[test]
    fn removed_indices_must_exist_once() {
        let removing = |removed: &str| {
            let json = format!(r#"{{"c":"BTC","t":2,"l":[[],[]],"r":[{removed},[]]}}"#);
            after_book(&change(&json))
        };
        assert!(matches!(
            removing("[3]"),
            Err(L2Error::Removed { index: 3, len: 3 })
        ));
        for (removed, index) in [("[0,0]", 0), ("[1,2,1]", 1)] {
            assert!(
                matches!(removing(removed), Err(L2Error::RemovedTwice { index: i }) if i == index),
                "{removed}"
            );
        }
    }

    #[test]
    fn a_book_out_of_order_is_rejected() {
        for levels in [
            r#"[[{"px":"99.0","sz":"1","n":1},{"px":"100.0","sz":"1","n":1}],[]]"#,
            r#"[[],[{"px":"102.0","sz":"1","n":1},{"px":"101.0","sz":"1","n":1}]]"#,
            r#"[[{"px":"100.0","sz":"1","n":1},{"px":"100.0","sz":"2","n":1}],[]]"#,
        ] {
            let text = format!(r#"{{"coin":"BTC","time":1,"levels":{levels}}}"#);
            let wire = WireL2 {
                s: Some(serde_json::from_str(&text).expect("fixture parses")),
                c: None,
            };
            let result = L2Decoder::new().apply(&wire, &market()).cloned();
            assert!(matches!(result, Err(L2Error::Unsorted)), "{levels}");
        }
    }

    #[test]
    fn a_message_holds_exactly_one_of_book_or_change() {
        let mut both = book();
        both.c = change(r#"{}"#).c;
        for wire in [WireL2 { s: None, c: None }, both] {
            let result = after_book(&wire);
            assert!(matches!(result, Err(L2Error::Shape)));
        }
    }

    #[test]
    fn a_change_must_decode() {
        let not_base64 = WireL2 {
            s: None,
            c: Some("not base64!".to_string()),
        };
        assert!(matches!(after_book(&not_base64), Err(L2Error::Base64(_))));
        let not_deflate = WireL2 {
            s: None,
            c: Some(STANDARD.encode([0xff_u8; 16])),
        };
        assert!(matches!(after_book(&not_deflate), Err(L2Error::Inflate(_))));
        assert!(matches!(
            after_book(&change("not json")),
            Err(L2Error::Json(_))
        ));
        assert!(matches!(
            after_book(&change(r#"{"c":"BTC","t":2,"l":[[]],"r":[[],[]]}"#)),
            Err(L2Error::Json(_))
        ));
    }

    #[test]
    fn a_change_is_bounded_in_size() {
        let at_limit = vec![b' '; MAX_CHANGE_BYTES as usize];
        assert!(matches!(
            after_book(&change(std::str::from_utf8(&at_limit).expect("ascii"))),
            Err(L2Error::Json(_))
        ));
        let over = vec![b' '; MAX_CHANGE_BYTES as usize + 1];
        assert!(matches!(
            after_book(&change(std::str::from_utf8(&over).expect("ascii"))),
            Err(L2Error::TooLarge)
        ));
    }

    #[test]
    fn a_change_checks_coin_time_and_numbers() {
        let cases = [
            (r#"{"c":"ETH","t":2,"l":[[],[]],"r":[[],[]]}"#, "coin"),
            (
                r#"{"c":"BTC","t":9007199254741,"l":[[],[]],"r":[[],[]]}"#,
                "time",
            ),
            (
                r#"{"c":"BTC","t":2,"l":[[{"p":"x","s":"1"}],[]],"r":[[],[]]}"#,
                "px",
            ),
            (
                r#"{"c":"BTC","t":2,"l":[[],[{"p":"1","s":"-1"}]],"r":[[],[]]}"#,
                "sz",
            ),
        ];
        for (json, field) in cases {
            let result = after_book(&change(json));
            let ok = match field {
                "coin" => matches!(result, Err(L2Error::Convert(ConvertError::Coin(_)))),
                "time" => matches!(result, Err(L2Error::Convert(ConvertError::TimeOverflow(_)))),
                "px" => matches!(result, Err(L2Error::Convert(ConvertError::Px(_)))),
                _ => matches!(result, Err(L2Error::Convert(ConvertError::Sz(_)))),
            };
            assert!(ok, "{field}");
        }
    }

    #[test]
    fn the_channel_parses_as_a_message() {
        let text = r#"{"channel":"l2","data":{"c":"abc"}}"#;
        assert_eq!(
            serde_json::from_str::<Message>(text).expect("parses"),
            Message::L2(WireL2 {
                s: None,
                c: Some("abc".to_string())
            })
        );
        let duplicate = r#"{"channel":"l2","data":{"c":"abc","c":"abd"}}"#;
        assert!(serde_json::from_str::<Message>(duplicate).is_err());
    }
}
