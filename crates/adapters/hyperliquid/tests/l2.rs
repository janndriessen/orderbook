//! Replays recorded `l2` messages and compares the result with a fresh
//! subscription's book at the same push.
//!
//! Both fixtures were recorded from the public WebSocket at the finest price
//! grouping: `l2-stream.jsonl` is one subscription's first book and the
//! changes after it, and `l2-fresh-snapshot.json` is the first book of a second
//! subscription, opened later, whose time equals the last change's.

use hyperliquid_adapter::{L2Decoder, Message};
use protocol::MarketInfo;

const STREAM: &str = include_str!("fixtures/l2-stream.jsonl");
const FRESH: &str = include_str!("fixtures/l2-fresh-snapshot.json");

fn market() -> MarketInfo {
    MarketInfo {
        symbol: "BTC-USD".to_string(),
        price_decimals: 0,
        qty_decimals: 5,
    }
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
    // Changes carry no order counts, so only prices and sizes are compared.
    let shape = |levels: &[hyperliquid_adapter::BookLevel]| -> Vec<(i64, u64)> {
        levels
            .iter()
            .map(|level| (level.price, level.qty))
            .collect()
    };
    assert_eq!(shape(&ours.bids), shape(&fresh.bids));
    assert_eq!(shape(&ours.asks), shape(&fresh.asks));
    assert_eq!((ours.bids.len(), ours.asks.len()), (20, 20));
}
