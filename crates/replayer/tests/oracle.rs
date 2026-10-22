//! Replays a recorded session and compares the book with the one the source
//! reported.
//!
//! The fixture is 40 seconds of public BTC market data, recorded from
//! Hyperliquid with the `users` fields removed. It holds both book feeds:
//! full snapshots of 20 levels per side and top snapshots of 5.
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
/// Levels per side that a top snapshot carries; a snapshot with more is full.
const TOP_DEPTH: usize = 5;
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
    full: bool,
    bids: Levels,
    asks: Levels,
}

struct Trade {
    ts: u64,
    side: String,
    price: i64,
    qty: u64,
}

fn recorded<'a>(text: &'a str, channel: &'a str) -> impl Iterator<Item = Value> + use<'a> {
    text.lines()
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

fn snapshots(text: &str) -> Vec<Snapshot> {
    let mut all: Vec<Snapshot> = recorded(text, "l2Book")
        .map(|m| Snapshot {
            ts: m["data"]["time"].as_u64().expect("time") * 1_000,
            full: m["data"]["levels"]
                .as_array()
                .expect("levels")
                .iter()
                .any(|side| side.as_array().expect("levels").len() > TOP_DEPTH),
            bids: side_levels(&m["data"]["levels"][0]),
            asks: side_levels(&m["data"]["levels"][1]),
        })
        .collect();
    all.sort_by_key(|s| s.ts);
    all
}

fn trades(text: &str) -> Vec<Trade> {
    let mut all: Vec<Trade> = recorded(text, "trades")
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

/// Replays `text` and compares the matcher with every snapshot it holds;
/// returns how many full and top snapshots were checked.
fn check_every_snapshot(text: &str) -> (usize, usize) {
    let loaded = replayer::load(text, &engine::market()).expect("load");
    let mut matcher = Matcher::new();
    let mut commands = loaded.commands.into_iter().peekable();
    let snapshots = snapshots(text);
    // The replay starts at the first full snapshot.
    let start = snapshots
        .iter()
        .position(|s| s.full)
        .expect("a full snapshot");
    let (mut full, mut tops) = (0, 0);
    for snapshot in &snapshots[start..] {
        while let Some((_, command)) = commands.next_if(|(ts, _)| *ts <= snapshot.ts) {
            matcher.apply(command);
        }
        let (bids, asks) = (
            book_levels(&matcher, Side::Buy),
            book_levels(&matcher, Side::Sell),
        );
        if snapshot.full {
            assert_eq!(bids, snapshot.bids, "bids at {}", snapshot.ts);
            assert_eq!(asks, snapshot.asks, "asks at {}", snapshot.ts);
            full += 1;
        } else {
            assert_eq!(
                best(&bids, Side::Buy, TOP_DEPTH),
                best(&snapshot.bids, Side::Buy, TOP_DEPTH)
            );
            assert_eq!(
                best(&asks, Side::Sell, TOP_DEPTH),
                best(&snapshot.asks, Side::Sell, TOP_DEPTH)
            );
            tops += 1;
        }
    }
    (full, tops)
}

#[test]
fn every_snapshot_is_rebuilt_exactly() {
    let (full, tops) = check_every_snapshot(FIXTURE);
    println!("{full} full and {tops} top snapshots checked");
    assert!(full > 5 && tops > 5);
}

/// The book the feed should end on, from a plain level model applied in time
/// order (trades before a snapshot of the same time): a full snapshot replaces
/// both sides, a top snapshot replaces only the price window it spans on each
/// side (an empty side changes nothing), and a trade takes its size off the
/// level at its price on the side it hit. Events before the first full
/// snapshot are ignored, as the replay drops them.
fn expected_final_book(text: &str) -> (Levels, Levels) {
    enum Event {
        Trade(Trade),
        Snapshot(Snapshot),
    }
    let mut events: Vec<(u64, u8, Event)> = trades(text)
        .into_iter()
        .map(|t| (t.ts, 0, Event::Trade(t)))
        .chain(
            snapshots(text)
                .into_iter()
                .map(|s| (s.ts, 1, Event::Snapshot(s))),
        )
        .collect();
    events.sort_by_key(|&(ts, order, _)| (ts, order));
    let (mut bids, mut asks) = (Levels::new(), Levels::new());
    let mut started = false;
    for (_, _, event) in events {
        match event {
            Event::Snapshot(snapshot) if snapshot.full => {
                started = true;
                (bids, asks) = (snapshot.bids, snapshot.asks);
            }
            Event::Snapshot(top) if started => {
                if let Some(&lowest) = top.bids.keys().next() {
                    bids.retain(|&price, _| price < lowest);
                    bids.extend(&top.bids);
                }
                if let Some(&highest) = top.asks.keys().next_back() {
                    asks.retain(|&price, _| price > highest);
                    asks.extend(&top.asks);
                }
            }
            Event::Trade(trade) if started => {
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
            _ => {}
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

/// The best `depth` levels of one side, best first.
fn best(levels: &Levels, side: Side, depth: usize) -> Vec<(i64, u64)> {
    let all = levels.iter().map(|(&price, &qty)| (price, qty));
    match side {
        Side::Buy => all.rev().take(depth).collect(),
        Side::Sell => all.take(depth).collect(),
    }
}

/// Serves `text` through the engine and compares the feed's final book with
/// the level model.
async fn check_feed_end(text: &str) {
    let loaded = replayer::load(text, &engine::market()).expect("load");
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

    let (expected_bids, expected_asks) = expected_final_book(text);
    assert!(updates > 0);
    assert_eq!(bids, expected_bids);
    assert_eq!(asks, expected_asks);
}

#[tokio::test]
async fn the_feed_ends_on_the_source_book() {
    check_feed_end(FIXTURE).await;
}

/// A full snapshot, a sell trade that reduces bid 100, then a top snapshot
/// whose bid window (110 to 106) leaves bid 100 outside and whose ask window
/// (up to 115) holds the stale asks 101 to 110, and a buy trade inside it.
fn trade_outside_a_later_top_window() -> String {
    let level = |price: u32, size: &str| format!(r#"{{"n":1,"px":"{price}.0","sz":"{size}"}}"#);
    let side = |prices: std::ops::RangeInclusive<u32>| {
        let levels: Vec<String> = prices.map(|price| level(price, "1.0")).collect();
        levels.join(",")
    };
    let book = |time: u32, bids: String, asks: String| {
        format!(
            r#"{{"channel":"l2Book","data":{{"coin":"BTC","time":{time},"levels":[[{bids}],[{asks}]]}}}}"#
        )
    };
    let trade = |time: u32, side: &str, price: u32, size: &str, tid: u32| {
        format!(
            r#"{{"channel":"trades","data":[{{"coin":"BTC","side":"{side}","px":"{price}.0","sz":"{size}","time":{time},"hash":"0x00","tid":{tid}}}]}}"#
        )
    };
    let descending = |high: u32, low: u32| {
        let levels: Vec<String> = (low..=high).rev().map(|p| level(p, "1.0")).collect();
        levels.join(",")
    };
    [
        book(1_000, descending(100, 81), side(101..=120)),
        trade(1_100, "A", 100, "0.4", 1),
        book(1_200, descending(110, 106), side(111..=115)),
        trade(1_300, "B", 111, "0.5", 2),
    ]
    .join("\n")
        + "\n"
}

#[tokio::test]
async fn a_trade_outside_a_later_top_window_stays_in_the_book() {
    let text = trade_outside_a_later_top_window();
    assert_eq!(check_every_snapshot(&text), (1, 1));
    let (bids, asks) = expected_final_book(&text);
    // Bid 100 keeps the reduction from the trade; ask 111 the later one.
    assert_eq!(bids.get(&100), Some(&60_000));
    assert_eq!(asks.get(&111), Some(&50_000));
    assert_eq!(asks.keys().next(), Some(&111));
    check_feed_end(&text).await;
}
