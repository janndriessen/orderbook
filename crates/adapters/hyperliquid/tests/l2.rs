//! Replays recorded `l2` messages and compares the result with other views of
//! the same book at the same push.
//!
//! The fixtures were recorded from the public WebSocket at the finest price
//! grouping: `l2-stream.jsonl` is one subscription's first book and the
//! changes after it, and `l2-fresh-snapshot.json` is the first book of a second
//! subscription, opened later, whose time equals the last change's.
//! `l2-and-l2book.jsonl` holds another subscription's `l2` messages, with
//! uncompressed changes among them, and the documented 20-level `l2Book`
//! snapshots recorded on the same connection. It ends at a push that was an
//! uncompressed change and has a documented book, so a decoder that skipped
//! uncompressed changes would fail there.

use std::collections::HashMap;

use hyperliquid_adapter::{BookLevel, BookSnapshot, L2Decoder, Message};
use protocol::MarketInfo;

const STREAM: &str = include_str!("fixtures/l2-stream.jsonl");
const FRESH: &str = include_str!("fixtures/l2-fresh-snapshot.json");
const WITH_L2BOOK: &str = include_str!("fixtures/l2-and-l2book.jsonl");

fn market() -> MarketInfo {
    MarketInfo {
        symbol: "BTC-USD".to_string(),
        price_decimals: 0,
        qty_decimals: 5,
    }
}

/// Prices and sizes, the part of a level both feeds report; changes carry no
/// order counts.
fn shape(levels: &[BookLevel]) -> Vec<(i64, u64)> {
    levels
        .iter()
        .map(|level| (level.price, level.qty))
        .collect()
}

fn l2(text: &str) -> hyperliquid_adapter::WireL2 {
    match serde_json::from_str(text).expect("recorded line parses") {
        Message::L2(wire) => wire,
        other => panic!("expected an l2 message, got {other:?}"),
    }
}

#[test]
fn replayed_changes_give_the_book_of_a_fresh_subscription() {
    let mut decoder = L2Decoder::new();
    let mut changes = 0;
    let mut last = None;
    for line in STREAM.lines() {
        let wire = l2(line);
        changes += usize::from(wire.c.is_some());
        last = Some(
            decoder
                .apply(&wire, &market())
                .expect("line applies")
                .clone(),
        );
    }
    let ours = last.expect("the stream has lines");

    let mut fresh_decoder = L2Decoder::new();
    let fresh = fresh_decoder
        .apply(&l2(FRESH), &market())
        .expect("fresh book applies");

    assert_eq!(changes, 9, "the fixture is a book and nine changes");
    assert_eq!(ours.ts, fresh.ts);
    assert_eq!(shape(&ours.bids), shape(&fresh.bids));
    assert_eq!(shape(&ours.asks), shape(&fresh.asks));
    assert_eq!((ours.bids.len(), ours.asks.len()), (20, 20));
}

#[test]
fn replayed_changes_give_the_documented_book_at_the_same_push() {
    let mut decoder = L2Decoder::new();
    let mut books: HashMap<u64, BookSnapshot> = HashMap::new();
    let mut documented = Vec::new();
    let mut uncompressed = Vec::new();
    for line in WITH_L2BOOK.lines() {
        match serde_json::from_str(line).expect("recorded line parses") {
            Message::L2(wire) => {
                let book = decoder.apply(&wire, &market()).expect("line applies");
                if wire.u.is_some() {
                    uncompressed.push(book.ts);
                }
                books.insert(book.ts, book.clone());
            }
            Message::L2Book(book) => {
                documented.push(book.to_snapshot(&market()).expect("converts"))
            }
            other => panic!("unexpected message {other:?}"),
        }
    }
    assert_eq!(
        uncompressed.len(),
        2,
        "the fixture has two uncompressed changes"
    );
    assert_eq!(
        documented.len(),
        3,
        "the fixture has three documented books"
    );
    let last = documented.last().map(|book| book.ts);
    assert_eq!(
        last,
        uncompressed.last().copied(),
        "the last documented book is at an uncompressed change"
    );
    for book in &documented {
        let ours = books
            .get(&book.ts)
            .expect("a push at the documented book's time");
        assert_eq!(shape(&ours.bids), shape(&book.bids), "bids at {}", book.ts);
        assert_eq!(shape(&ours.asks), shape(&book.asks), "asks at {}", book.ts);
    }
}
