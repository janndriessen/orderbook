//! Replays a recorded session and compares the book with the one the source
//! reported.
//!
//! The fixture is 40 seconds of public BTC market data, recorded from
//! Hyperliquid with the `users` fields removed.
//!
//! The test deliberately parses the raw JSON with its own decimal conversion,
//! so it shares no code with the adapter's parser or the translator it checks.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use futures_util::StreamExt;
use matching::Matcher;
use protocol::{FeedMessage, Level, Side};
use replayer::Speed;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const FIXTURE: &str = include_str!("fixtures/hyperliquid-btc-40s.jsonl");
/// How long the service may stay silent before the test gives up instead of
/// hanging.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Lots per coin: the market's `qty_decimals` is 5, prices are whole ticks.
const LOT_DECIMALS: usize = 5;

type Levels = BTreeMap<i64, u64>;

/// A decimal string as an integer scaled by 10^`decimals`, without floats.
fn scaled(text: &str, decimals: usize) -> u64 {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let fraction = fraction.trim_end_matches('0');
    assert!(fraction.len() <= decimals, "{text} has too many decimals");
    format!("{whole}{fraction:0<decimals$}")
        .parse()
        .expect(text)
}

struct Snapshot {
    /// Microseconds, like the commands' times.
    ts: u64,
    bids: Levels,
    asks: Levels,
}

struct Trade {
    ts: u64,
    side: String,
    price: i64,
    qty: u64,
}

fn recorded(channel: &str) -> impl Iterator<Item = Value> + use<'_> {
    FIXTURE
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("fixture line"))
        .filter(move |value| value["channel"] == channel)
}

fn side_levels(levels: &Value) -> Levels {
    levels
        .as_array()
        .expect("levels")
        .iter()
        .map(|l| {
            let price = scaled(l["px"].as_str().expect("px"), 0) as i64;
            (price, scaled(l["sz"].as_str().expect("sz"), LOT_DECIMALS))
        })
        .filter(|&(_, qty)| qty > 0)
        .collect()
}

fn snapshots() -> Vec<Snapshot> {
    let mut all: Vec<Snapshot> = recorded("l2Book")
        .map(|m| Snapshot {
            ts: m["data"]["time"].as_u64().expect("time") * 1_000,
            bids: side_levels(&m["data"]["levels"][0]),
            asks: side_levels(&m["data"]["levels"][1]),
        })
        .collect();
    all.sort_by_key(|s| s.ts);
    all
}

fn trades() -> Vec<Trade> {
    let mut all: Vec<Trade> = recorded("trades")
        .flat_map(|m| m["data"].as_array().expect("trades").clone())
        .map(|t| Trade {
            ts: t["time"].as_u64().expect("time") * 1_000,
            side: t["side"].as_str().expect("side").to_owned(),
            price: scaled(t["px"].as_str().expect("px"), 0) as i64,
            qty: scaled(t["sz"].as_str().expect("sz"), LOT_DECIMALS),
        })
        .collect();
    all.sort_by_key(|t| t.ts);
    all
}

fn book_levels(matcher: &Matcher, side: Side) -> Levels {
    matcher
        .book()
        .levels(side)
        .map(|(price, level)| (price, level.total()))
        .collect()
}

#[test]
fn every_snapshot_is_rebuilt_exactly() {
    let loaded = replayer::load(FIXTURE, &engine::market()).expect("load");
    let mut matcher = Matcher::new();
    let mut commands = loaded.commands.into_iter().peekable();
    let snapshots = snapshots();
    for snapshot in &snapshots {
        while let Some((_, command)) = commands.next_if(|(ts, _)| *ts <= snapshot.ts) {
            matcher.apply(command);
        }
        assert_eq!(
            book_levels(&matcher, Side::Buy),
            snapshot.bids,
            "bids at {}",
            snapshot.ts
        );
        assert_eq!(
            book_levels(&matcher, Side::Sell),
            snapshot.asks,
            "asks at {}",
            snapshot.ts
        );
    }
    println!("{} snapshots checked", snapshots.len());
    assert!(snapshots.len() > 5);
}

/// The last snapshot with every later trade taken off the side it hit.
fn expected_final_book() -> (Levels, Levels) {
    let last = snapshots().pop().expect("a snapshot");
    let (mut bids, mut asks) = (last.bids, last.asks);
    for trade in trades().into_iter().filter(|t| t.ts > last.ts) {
        // A buy takes liquidity from the asks, a sell from the bids.
        let side = if trade.side == "B" {
            &mut asks
        } else {
            &mut bids
        };
        let left = side
            .get(&trade.price)
            .copied()
            .unwrap_or(0)
            .saturating_sub(trade.qty);
        if left == 0 {
            side.remove(&trade.price);
        } else {
            side.insert(trade.price, left);
        }
    }
    (bids, asks)
}

fn patch(side: &mut Levels, changes: &[Level]) {
    for Level { price, qty } in changes {
        if *qty == 0 {
            side.remove(price);
        } else {
            side.insert(*price, *qty);
        }
    }
}

#[tokio::test]
async fn the_feed_ends_on_the_source_book() {
    let loaded = replayer::load(FIXTURE, &engine::market()).expect("load");
    // The flow waits for the client, so it cannot finish and close the
    // service before anyone is connected.
    let (start, started) = mpsc::channel::<()>();
    let mut gate = Some(started);
    let flow = replayer::pace(loaded.commands, Speed::Max).inspect(move |_| {
        if let Some(gate) = gate.take() {
            let _ = gate.recv();
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let service = tokio::spawn(engine::run(
        listener,
        flow,
        engine::CancellationToken::new(),
        engine::PING_INTERVAL,
    ));
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");

    let (mut bids, mut asks) = (Levels::new(), Levels::new());
    let mut seq = None;
    let mut updates = 0;
    loop {
        let frame = tokio::time::timeout(READ_TIMEOUT, client.next())
            .await
            .expect("the service sent nothing for too long");
        let Some(frame) = frame else { break };
        let Message::Text(text) = frame.expect("frame") else {
            continue;
        };
        match serde_json::from_str(text.as_str()).expect("feed message") {
            FeedMessage::Snapshot {
                seq: first,
                bids: b,
                asks: a,
                ..
            } => {
                assert!(seq.is_none(), "a second snapshot");
                seq = Some(first);
                bids = b.iter().map(|l| (l.price, l.qty)).collect();
                asks = a.iter().map(|l| (l.price, l.qty)).collect();
                start.send(()).expect("flow waiting");
            }
            FeedMessage::Update {
                seq: next,
                bids: b,
                asks: a,
                ..
            } => {
                assert_eq!(Some(next), seq.map(|s| s + 1), "gap in the feed");
                seq = Some(next);
                patch(&mut bids, &b);
                patch(&mut asks, &a);
                updates += 1;
            }
            FeedMessage::Heartbeat { .. } => {}
        }
    }
    service
        .await
        .expect("service task")
        .expect("service result");

    let (expected_bids, expected_asks) = expected_final_book();
    assert!(updates > 0);
    assert_eq!(bids, expected_bids);
    assert_eq!(asks, expected_asks);
}
