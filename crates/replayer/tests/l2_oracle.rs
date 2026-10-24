//! Serves the recording of the `l2` feed through the real service and follows
//! the feed as a client, checking the client's book against what the source
//! showed at every complete push.
//!
//! The flow is released one push at a time, and the next push only after the
//! client has received all of the previous push's events (the publisher
//! batches by tick, not by command, so the update boundaries say nothing). The
//! engine is deterministic, so the number of feed events a push produces is
//! known beforehand by running its commands through a local matcher; once the
//! client has that many in total, its book is the book as of the push. An
//! update's events carry the time of their command, which is the push's time.
//! At each push the client's book must equal the merge of the six groupings,
//! and grouped by each grouping's own rule must equal that grouping's decoded
//! view.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use hyperliquid_adapter::{BookLevel, BookSnapshot, Grouping, L2Decoder, Message, merge};
use matching::Matcher;
use protocol::{Command, Event, FeedMessage, Level, Price, Side};
use replayer::{Signal, Speed};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as Frame;

const FIXTURE: &str = include_str!("fixtures/hyperliquid-btc-l2-10s.jsonl");
/// How long the service may stay silent before the test gives up instead of
/// hanging.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the whole flow may take before the test gives up.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Buckets per side that every grouping shows.
const WINDOW: usize = 20;
/// Price groupings the recorder subscribes to.
const GROUPINGS: usize = 6;
/// Complete pushes in the fixture: pushes at which all six groupings sent a
/// book, counted from its lines with a separate script.
const COMPLETE_PUSHES: usize = 18;

type Levels = BTreeMap<Price, u64>;
type Views = BTreeMap<Grouping, BookSnapshot>;

fn as_levels(levels: &[BookLevel]) -> Levels {
    levels.iter().map(|l| (l.price, l.qty)).collect()
}

/// The books each grouping showed, by push time, as the adapter's decoder
/// rebuilt them. Only pushes at which all six groupings sent a book are kept.
fn complete_pushes() -> BTreeMap<u64, Views> {
    let mut decoders: BTreeMap<Grouping, L2Decoder> = BTreeMap::new();
    let mut pushes: BTreeMap<u64, Views> = BTreeMap::new();
    for text in FIXTURE.lines() {
        let line: Value = serde_json::from_str(text).expect("fixture line");
        if line["channel"] != "l2" {
            continue;
        }
        let subscription = &line["subscription"];
        let key = Grouping {
            sig_figs: Some(subscription["s"].as_u64().expect("s") as u32),
            mantissa: subscription["m"].as_u64().unwrap_or(1) as u32,
        };
        let Message::L2(wire) = serde_json::from_str(text).expect("l2 message") else {
            panic!("not an l2 message");
        };
        let book = decoders
            .entry(key)
            .or_default()
            .apply(&wire, &engine::market())
            .expect("fixture applies");
        pushes.entry(book.ts).or_default().insert(key, book.clone());
    }
    pushes.retain(|_, views| views.len() == GROUPINGS);
    pushes
}

/// The levels of one side grouped by a grouping's rule: buckets of `width`
/// ticks, bids labeled by rounding down and asks up, the best `WINDOW`.
fn bucketed(levels: &Levels, side: Side, width: Price) -> Levels {
    let mut buckets = Levels::new();
    for (&price, &qty) in levels {
        let label = match side {
            Side::Buy => price.div_euclid(width) * width,
            Side::Sell => (price + width - 1).div_euclid(width) * width,
        };
        *buckets.entry(label).or_default() += qty;
    }
    let ordered: Vec<(Price, u64)> = match side {
        Side::Buy => buckets.into_iter().rev().collect(),
        Side::Sell => buckets.into_iter().collect(),
    };
    ordered.into_iter().take(WINDOW).collect()
}

/// Compares the client's book with the merge of the groupings' views and with
/// each view grouped by its own rule.
fn check_book(bids: &Levels, asks: &Levels, views: &Views, ts: u64) {
    let inputs: Vec<(Grouping, &BookSnapshot)> = views
        .iter()
        .map(|(&grouping, book)| (grouping, book))
        .collect();
    let merged = merge(&inputs).expect("merges").book;
    assert_eq!(bids, &as_levels(&merged.bids), "bids at {ts}");
    assert_eq!(asks, &as_levels(&merged.asks), "asks at {ts}");

    // One bucket width per book, from the number of digits of the best bid.
    let digits = bids.keys().next_back().expect("a bid").ilog10() + 1;
    for (&Grouping { sig_figs, mantissa }, view) in views {
        let sig_figs = sig_figs.expect("the fixture has no grouping without rounding");
        let width = 10_i64.pow(digits - sig_figs) * Price::from(mantissa);
        assert_eq!(
            bucketed(bids, Side::Buy, width),
            as_levels(&view.bids),
            "bids of {sig_figs}/{mantissa} at {ts}"
        );
        assert_eq!(
            bucketed(asks, Side::Sell, width),
            as_levels(&view.asks),
            "asks of {sig_figs}/{mantissa} at {ts}"
        );
    }
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
async fn the_feed_shows_every_complete_push_and_every_grouping_of_it() {
    let pushes = complete_pushes();
    assert_eq!(pushes.len(), COMPLETE_PUSHES);
    let times: Vec<u64> = pushes.keys().copied().collect();

    let loaded = replayer::load(FIXTURE, &engine::market()).expect("load");
    let expected = expected_feed_events(&loaded.commands, &times);
    // The client releases each push once it has the previous one.
    let (release, signals) = mpsc::channel::<Signal>();
    let flow = replayer::step_on(replayer::pace(loaded.commands, Speed::Max), signals);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let service = tokio::spawn(engine::run(
        listener,
        flow,
        engine::CancellationToken::new(),
        engine::PING_INTERVAL,
        engine::MAX_CLIENTS,
    ));
    let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("connect");

    let (mut bids, mut asks) = (Levels::new(), Levels::new());
    let mut seq = None;
    // The push released last and still to be checked, and the feed events the
    // client has received in all.
    let mut awaiting = 0;
    let mut seen = 0;
    let deadline = Instant::now() + TOTAL_TIMEOUT;
    loop {
        // A push whose events never complete (heartbeats keep frames coming)
        // trips the deadline instead of waiting for ever.
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "push {awaiting} never completed");
        let frame = tokio::time::timeout(left.min(READ_TIMEOUT), client.next())
            .await
            .expect("the service sent nothing for too long, or the flow overran its time");
        let Some(frame) = frame else { break };
        let Frame::Text(text) = frame.expect("frame") else {
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
                assert!(b.is_empty() && a.is_empty(), "the flow has not started");
                seq = Some(first);
                release.send(Signal::Next).expect("flow waiting");
            }
            FeedMessage::Update {
                seq: next,
                bids: b,
                asks: a,
                events,
            } => {
                assert_eq!(Some(next), seq.map(|s| s + 1), "gap in the feed");
                seq = Some(next);
                // Only the released push is in flight, so every event is its
                // own and none can spill over into the next push.
                let ts = *times
                    .get(awaiting)
                    .expect("an update arrived after the last push was checked");
                assert!(
                    events.iter().all(|e| e.ts == ts),
                    "an event is not from the push at {ts}"
                );
                seen += events.len();
                assert!(seen <= expected[awaiting], "more events than counted");
                patch(&mut bids, &b);
                patch(&mut asks, &a);
                if seen == expected[awaiting] {
                    check_book(&bids, &asks, &pushes[&ts], ts);
                    awaiting += 1;
                    if awaiting < times.len() {
                        release.send(Signal::Next).expect("flow waiting");
                    }
                }
            }
            FeedMessage::Heartbeat { .. } => {}
        }
    }
    // Checked before the service is awaited, which only ends once the flow
    // has: dropping the release handle lets a parked flow end if the client
    // stopped early.
    assert_eq!(awaiting, COMPLETE_PUSHES, "pushes checked");
    drop(release);
    tokio::time::timeout(READ_TIMEOUT, service)
        .await
        .expect("the service did not stop")
        .expect("service task")
        .expect("service result");
}

/// For each push time, the feed events of all pushes up to and including it:
/// what a deterministic engine emits for the push's commands, less the level
/// updates, which the feed turns into level changes. The client has a push in
/// full once it has received that many.
fn expected_feed_events(commands: &[(u64, Command)], times: &[u64]) -> Vec<usize> {
    let mut matcher = Matcher::new();
    let mut total = 0;
    let mut cumulative = Vec::new();
    for &ts in times {
        for (_, command) in commands.iter().filter(|(t, _)| *t == ts) {
            let events = matcher.apply(*command);
            // A command's level changes reach the client in the update that
            // holds its other events only if those come first; otherwise the
            // count could complete before the last level change arrived.
            assert!(
                !matches!(events.first(), None | Some(Event::L2Update { .. })),
                "a command's first event is its acknowledgement or rejection"
            );
            total += events
                .iter()
                .filter(|event| !matches!(event, Event::L2Update { .. }))
                .count();
        }
        cumulative.push(total);
    }
    assert!(
        cumulative.windows(2).all(|w| w[0] < w[1]) && cumulative[0] > 0,
        "every push has events"
    );
    cumulative
}
