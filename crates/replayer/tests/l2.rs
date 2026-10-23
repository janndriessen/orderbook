//! Replays a recording of the `l2` feed at its six price groupings and compares
//! the matcher's book with what the source showed at every push.
//!
//! The fixture is public BTC market data recorded from Hyperliquid: the first
//! messages of all six groupings (one connection each, so one book and its
//! changes per grouping) up to ten seconds after the last of them sent its
//! first book, and the trades in that time. Its books are decoded here with the
//! adapter's `l2` decoder, the one piece this test shares with the loader. The
//! merge is checked from the other side: the matcher's book, grouped by each
//! grouping's own rule, has to give that grouping's decoded book.

use std::collections::BTreeMap;

use hyperliquid_adapter::{BookLevel, BookSnapshot, Grouping, L2Decoder, Message, merge};
use matching::Matcher;
use protocol::{Command, OrderType, Price, Side, TimeInForce};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/hyperliquid-btc-l2-10s.jsonl");
/// Buckets per side that every grouping shows.
const WINDOW: usize = 20;
/// Price groupings the recorder subscribes to.
const GROUPINGS: usize = 6;

type Levels = BTreeMap<Price, u64>;

fn book_levels(matcher: &Matcher, side: Side) -> Levels {
    matcher
        .book()
        .levels(side)
        .map(|(price, level)| (price, level.total()))
        .collect()
}

fn as_levels(levels: &[BookLevel]) -> Levels {
    levels.iter().map(|l| (l.price, l.qty)).collect()
}

/// The books each grouping showed, by push time, as the decoder rebuilt them.
fn pushes() -> BTreeMap<u64, BTreeMap<Grouping, BookSnapshot>> {
    let mut decoders: BTreeMap<Grouping, L2Decoder> = BTreeMap::new();
    let mut pushes: BTreeMap<u64, BTreeMap<Grouping, BookSnapshot>> = BTreeMap::new();
    for text in FIXTURE.lines() {
        let line: Value = serde_json::from_str(text).expect("fixture line");
        if line["channel"] != "l2" {
            continue;
        }
        let subscription = &line["subscription"];
        let sig_figs = subscription["s"].as_u64().expect("s") as u32;
        let mantissa = subscription["m"].as_u64().unwrap_or(1) as u32;
        let key = Grouping { sig_figs, mantissa };
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

#[test]
fn the_matcher_shows_every_complete_push_and_every_grouping_of_it() {
    let loaded = replayer::load(FIXTURE, &engine::market()).expect("loads");
    let pushes = pushes();
    let complete = complete_pushes();
    assert_eq!(complete.len(), 18, "complete pushes in the fixture");
    assert_eq!(
        loaded.startup_pushes + loaded.skipped_pushes,
        pushes.len() - complete.len()
    );
    assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (5, 2));
    assert_eq!(loaded.trailing_pushes, 0);
    assert_eq!(loaded.failed_merges, 0);
    assert_eq!(loaded.conflicting_qty, 0);
    // The book changes only at pushes.
    assert!(loaded.commands.iter().all(|(ts, _)| complete.contains(ts)));

    let mut matcher = Matcher::new();
    let mut commands = loaded.commands.into_iter().peekable();
    for ts in &complete {
        while let Some((_, command)) = commands.next_if(|(command_ts, _)| command_ts <= ts) {
            matcher.apply(command);
        }
        let group = &pushes[ts];
        let inputs: Vec<(Grouping, &BookSnapshot)> = group
            .iter()
            .map(|(&grouping, book)| (grouping, book))
            .collect();
        let merged = merge(&inputs).expect("merges").book;
        let (bids, asks) = (
            book_levels(&matcher, Side::Buy),
            book_levels(&matcher, Side::Sell),
        );
        assert_eq!(bids, as_levels(&merged.bids), "bids at {ts}");
        assert_eq!(asks, as_levels(&merged.asks), "asks at {ts}");

        let best_bid = *bids.keys().next_back().expect("a bid");
        let digits = best_bid.ilog10() + 1;
        for (&Grouping { sig_figs, mantissa }, view) in group {
            let width = 10_i64.pow(digits - sig_figs) * Price::from(mantissa);
            assert_eq!(
                bucketed(&bids, Side::Buy, width),
                as_levels(&view.bids),
                "bids of {sig_figs}/{mantissa} at {ts}"
            );
            assert_eq!(
                bucketed(&asks, Side::Sell, width),
                as_levels(&view.asks),
                "asks of {sig_figs}/{mantissa} at {ts}"
            );
        }
    }
    assert!(commands.next().is_none(), "no command after the last push");
}

/// The times of the complete pushes, in microseconds.
fn complete_pushes() -> Vec<u64> {
    pushes()
        .into_iter()
        .filter(|(_, group)| group.len() == GROUPINGS)
        .map(|(ts, _)| ts)
        .collect()
}

#[test]
fn trades_before_the_first_push_and_after_the_last_are_dropped() {
    let loaded = replayer::load(FIXTURE, &engine::market()).expect("loads");
    let complete = complete_pushes();
    let (first, last) = (complete[0], complete[complete.len() - 1]);
    let times: Vec<u64> = FIXTURE
        .lines()
        .map(|text| serde_json::from_str::<Value>(text).expect("fixture line"))
        .filter(|line| line["channel"] == "trades")
        .flat_map(|line| line["data"].as_array().expect("trades").clone())
        .map(|trade| trade["time"].as_u64().expect("time") * 1_000)
        .collect();
    let before = times.iter().filter(|&&ts| ts <= first).count();
    let after = times.iter().filter(|&&ts| ts > last).count();
    assert_eq!((times.len(), before, after), (37, 31, 0));
    assert_eq!(loaded.dropped_trades, before);
    assert_eq!(loaded.dropped_late_trades, after);
    assert_eq!(loaded.skipped_book_lines, 0);

    // Every trade in between is translated, once, as an IOC order: nothing
    // else in the replay is.
    let held = times.len() - before - after;
    assert_eq!(held, 6);
    let taker_orders = loaded
        .commands
        .iter()
        .filter(|(_, command)| {
            matches!(
                command,
                Command::Place {
                    order_type: OrderType::Limit {
                        time_in_force: TimeInForce::Ioc,
                        ..
                    },
                    ..
                }
            )
        })
        .count();
    assert_eq!(taker_orders, held);
}
