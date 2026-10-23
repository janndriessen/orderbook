//! Loads a recorded Hyperliquid session as commands and paces them into steps
//! for the service.

use std::iter;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use engine::Step;
use hyperliquid_adapter::{BookSnapshot, ConvertError, Divergence, Message, Trade, Translator};
use protocol::{Command, MarketInfo, Ts};
use tracing::info;

/// How fast recorded order flow is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// Waits equal the recorded gaps between commands.
    RealTime,
    /// Recorded gaps divided by the factor, at nanosecond precision.
    Times(NonZeroU32),
    /// No waits: commands are submitted as fast as the service takes them.
    Max,
    /// No waits either; [`step_on`] holds each recorded moment back until it
    /// is told to release it.
    Step,
}

/// The text is none of `max`, `step` or a positive integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("speed must be `max`, `step` or a positive integer")]
pub struct InvalidSpeed;

impl FromStr for Speed {
    type Err = InvalidSpeed;

    /// `max`, `step`, or a positive integer factor, where `1` is real time.
    fn from_str(text: &str) -> Result<Self, InvalidSpeed> {
        match text {
            "max" => return Ok(Self::Max),
            "step" => return Ok(Self::Step),
            _ => {}
        }
        match text.parse::<NonZeroU32>().map_err(|_| InvalidSpeed)? {
            factor if factor.get() == 1 => Ok(Self::RealTime),
            factor => Ok(Self::Times(factor)),
        }
    }
}

/// Turns recorded `(ts, command)` pairs into steps paced at `speed`.
///
/// Each step's wait is the gap from the latest time played so far to the next
/// command's time. A command whose time is not past that latest time (recordings
/// can repeat or reorder times inside one block) therefore costs no wait, and a
/// reordered one does not stretch the gap after it. The last step has no wait.
/// Every step keeps its recorded time. The input is read lazily, one command
/// ahead.
pub fn pace(
    commands: impl IntoIterator<Item = (Ts, Command)>,
    speed: Speed,
) -> impl Iterator<Item = Step> {
    let mut commands = commands.into_iter().peekable();
    let mut latest: Ts = 0;
    iter::from_fn(move || {
        let (ts, command) = commands.next()?;
        latest = latest.max(ts);
        let gap = match commands.peek() {
            Some(&(next_ts, _)) => Duration::from_micros(next_ts.saturating_sub(latest)),
            None => Duration::ZERO,
        };
        let wait = match speed {
            Speed::RealTime => gap,
            Speed::Times(n) => gap / n.get(),
            Speed::Max | Speed::Step => Duration::ZERO,
        };
        Some(Step { command, ts, wait })
    })
}

/// What [`step_on`] is told to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Release the next recorded moment.
    Next,
    /// End the flow.
    Stop,
}

/// Yields the steps of `flow` one recorded moment at a time: all consecutive
/// steps with the same time form a group (it can mix a snapshot's commands and
/// trades), and before the first step of a group this blocks until `signals`
/// delivers [`Signal::Next`]. [`Signal::Stop`] or a dropped sender ends the
/// flow without releasing the group, for good. Steps inside a released group
/// have no wait.
///
/// Like [`hold`], the wait happens inside `next`, so only `signals` can wake
/// the consuming thread.
pub fn step_on(
    mut flow: impl Iterator<Item = Step>,
    signals: Receiver<Signal>,
) -> impl Iterator<Item = Step> {
    let mut released: Option<Ts> = None;
    iter::from_fn(move || {
        let step = flow.next()?;
        if released != Some(step.ts) {
            if signals.recv() != Ok(Signal::Next) {
                return None;
            }
            info!(ts = step.ts, "released");
            released = Some(step.ts);
        }
        Some(Step {
            wait: Duration::ZERO,
            ..step
        })
    })
    .fuse()
}

/// Yields the steps of `flow`, then, instead of ending, waits until `release`
/// receives a message or its sender is dropped. Ending a flow stops the
/// service, so this keeps the final book served.
///
/// The wait happens inside `next`, so the thread consuming the flow is parked
/// there and only `release` can wake it; the service's own stop signal is
/// checked between steps and never reaches it.
pub fn hold(
    mut flow: impl Iterator<Item = Step>,
    release: Receiver<()>,
) -> impl Iterator<Item = Step> {
    iter::from_fn(move || {
        if let Some(step) = flow.next() {
            return Some(step);
        }
        let _ = release.recv();
        None
    })
}

/// Why a recording could not be loaded. A recording is never cut short: the
/// first bad line fails the whole load.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The line is not a message of the expected shape.
    #[error("line {line}: {source}")]
    Parse {
        /// 1-based line number.
        line: usize,
        /// What the parser rejected.
        source: serde_json::Error,
    },
    /// The line is on a channel that is neither data nor known housekeeping,
    /// or has none.
    #[error("line {line}: unexpected channel {channel}")]
    Channel {
        /// 1-based line number.
        line: usize,
        /// The channel as written, or `(none)`.
        channel: String,
    },
    /// A price, size, time or coin on the line cannot be converted.
    #[error("line {line}: {source}")]
    Convert {
        /// 1-based line number.
        line: usize,
        /// What could not be converted.
        source: ConvertError,
    },
    /// The recording holds no full book snapshot, so there is nothing to serve.
    #[error("the recording holds no full l2Book snapshot")]
    NoSnapshot,
}

/// A recording translated into commands.
#[derive(Debug)]
pub struct Loaded {
    /// Commands with their venue times, in time order.
    pub commands: Vec<(Ts, Command)>,
    /// How far the translator's book is known to differ from the recorded one.
    pub divergence: Divergence,
    /// Trades at or before the first full snapshot's time, which were left out.
    pub dropped_trades: usize,
    /// Top-of-book snapshots, best bid and offer messages included, before
    /// the first full snapshot, which were left out.
    pub dropped_top_snapshots: usize,
    /// Best bid and offer messages with a bid at or above the ask, which were
    /// left out as unusable.
    pub skipped_bbos: usize,
    /// Whether the last line was cut off and left out, as happens while the
    /// recorder is still writing.
    pub incomplete_last_line: bool,
}

/// The most levels per side that a snapshot of only the top of the book
/// carries; the fast feed sends 5, the standard feed 20.
const TOP_DEPTH: usize = 5;

/// A recorded book or trade, ready to be translated.
enum Recorded {
    Trade(Trade),
    Full(BookSnapshot),
    Top(BookSnapshot),
}

impl Recorded {
    /// Sort key: by time, trades first, because a snapshot already reflects
    /// the trades up to its own time.
    fn key(&self) -> (Ts, u8) {
        match self {
            Self::Trade(trade) => (trade.ts, 0),
            Self::Full(snapshot) | Self::Top(snapshot) => (snapshot.ts, 1),
        }
    }
}

/// Translates a JSON Lines recording of `l2Book`, `bbo` and `trades` messages,
/// with prices and sizes converted for `market`.
///
/// The two book feeds send messages of the same shape, so the depth tells them
/// apart: a snapshot with at most [`TOP_DEPTH`] levels on each side is the top
/// of the book and is mirrored only inside the price window it spans; a deeper
/// one is a full snapshot. A full snapshot of a book that really has at most
/// that many levels per side is ambiguous and is treated as a top snapshot:
/// stale levels beyond its window linger until the next full snapshot, and a
/// side that is empty in it keeps its stale levels, which can make post-only
/// orders on the other side be rejected (counted in `rejected_makers`).
///
/// A `bbo` message is a top snapshot of one level per side and is mirrored the
/// same way and counts as one for dropping. One with a bid at or above the ask
/// is left out and counted in `skipped_bbos`.
///
/// Subscription acknowledgements and `pong` replies are skipped, as are empty
/// lines. Recorded order can differ from time order, so the data is sorted by
/// time (stable) before it is translated. Trades at or before the first full
/// snapshot's time are dropped: the first trades message after subscribing is
/// a backlog, and a snapshot already reflects every trade up to its time. Top
/// snapshots before the first full one are dropped too, so the replay always
/// starts from a full book.
///
/// A recording that does not end with a line break may end in the middle of a
/// line, because the recorder writes in blocks while it runs; such a last line
/// is left out when it does not parse.
///
/// # Errors
///
/// Returns a [`LoadError`] for the first line that is malformed, on another
/// channel, or not convertible, or if the recording has no full snapshot.
pub fn load(recording: &str, market: &MarketInfo) -> Result<Loaded, LoadError> {
    let unfinished_line = (!recording.ends_with('\n')).then(|| recording.lines().count());
    let mut incomplete_last_line = false;
    let mut skipped_bbos = 0;
    let mut recorded = Vec::new();
    for (index, text) in recording.lines().enumerate() {
        let line = index + 1;
        if text.trim().is_empty() {
            continue;
        }
        let parse = |source| LoadError::Parse { line, source };
        let channel = match serde_json::from_str::<LineHeader>(text) {
            Ok(_) if !text.trim_start().starts_with('{') => {
                // The header struct also reads a JSON array, whose first
                // element would pass for a channel.
                let channel = "(none)".to_owned();
                return Err(LoadError::Channel { line, channel });
            }
            Ok(header) => header.channel,
            Err(_) if Some(line) == unfinished_line => {
                incomplete_last_line = true;
                continue;
            }
            Err(source) => return Err(parse(source)),
        };
        if matches!(channel.as_deref(), Some("subscriptionResponse" | "pong")) {
            continue;
        }
        if !matches!(channel.as_deref(), Some("l2Book" | "bbo" | "trades")) {
            let channel = channel.unwrap_or_else(|| "(none)".to_owned());
            return Err(LoadError::Channel { line, channel });
        }
        let convert = |source| LoadError::Convert { line, source };
        match serde_json::from_str(text).map_err(parse)? {
            Message::L2Book(book) => {
                let snapshot = book.to_snapshot(market).map_err(convert)?;
                recorded.push(
                    if snapshot.bids.len().max(snapshot.asks.len()) <= TOP_DEPTH {
                        Recorded::Top(snapshot)
                    } else {
                        Recorded::Full(snapshot)
                    },
                );
            }
            Message::Bbo(bbo) => match bbo.to_snapshot(market) {
                Ok(snapshot) => recorded.push(Recorded::Top(snapshot)),
                Err(ConvertError::LockedOrCrossed) => skipped_bbos += 1,
                Err(source) => return Err(convert(source)),
            },
            Message::Trades(trades) => {
                for trade in &trades {
                    recorded.push(Recorded::Trade(trade.to_trade(market).map_err(convert)?));
                }
            }
            // The channel check above already turns `l2` lines away; they are
            // not replayed yet.
            Message::L2(_) => {
                let channel = "l2".to_owned();
                return Err(LoadError::Channel { line, channel });
            }
        }
    }
    recorded.sort_by_key(Recorded::key);
    let mut translator = Translator::new();
    let mut commands = Vec::new();
    let (mut dropped_trades, mut dropped_top_snapshots) = (0, 0);
    let mut started = false;
    for item in &recorded {
        match item {
            // Trades sort before a snapshot of the same time, so everything
            // before the first full snapshot is at or before its time.
            Recorded::Trade(_) if !started => dropped_trades += 1,
            Recorded::Top(_) if !started => dropped_top_snapshots += 1,
            Recorded::Trade(trade) => commands.extend(translator.on_trade(trade)),
            Recorded::Top(snapshot) => commands.extend(translator.on_top(snapshot)),
            Recorded::Full(snapshot) => {
                started = true;
                commands.extend(translator.on_snapshot(snapshot));
            }
        }
    }
    if !started {
        return Err(LoadError::NoSnapshot);
    }
    Ok(Loaded {
        commands,
        divergence: translator.divergence(),
        dropped_trades,
        dropped_top_snapshots,
        skipped_bbos,
        incomplete_last_line,
    })
}

/// The channel of a recorded line. The line itself is parsed as written by
/// the adapter's strict message type afterwards: reading it into a generic
/// JSON value first would keep only the last of repeated keys and replay a
/// value the adapter's own parser refuses. A repeated key is an error in the
/// fields the adapter's types name; fields they do not name, like `users`,
/// are skipped either way.
#[derive(serde::Deserialize)]
struct LineHeader {
    channel: Option<String>,
}

/// The gaps between consecutive commands, in recorded time, that exceed
/// `limit`, as `(index of the command before the gap, gap)`. A corrupt
/// timestamp would otherwise stall a replay without a word.
pub fn long_waits(commands: &[(Ts, Command)], limit: Duration) -> Vec<(usize, Duration)> {
    pace(commands.iter().copied(), Speed::RealTime)
        .enumerate()
        .filter(|(_, step)| step.wait > limit)
        .map(|(index, step)| (index, step.wait))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(timestamps: &[Ts]) -> Vec<(Ts, Command)> {
        timestamps
            .iter()
            .zip(1..)
            .map(|(&ts, order_id)| (ts, Command::Cancel { order_id }))
            .collect()
    }

    fn waits(timestamps: &[Ts], speed: Speed) -> Vec<Duration> {
        pace(flow(timestamps), speed).map(|s| s.wait).collect()
    }

    fn micros(values: &[u64]) -> Vec<Duration> {
        values.iter().map(|&v| Duration::from_micros(v)).collect()
    }

    #[test]
    fn real_time_waits_equal_the_gaps() {
        assert_eq!(
            waits(&[0, 1_000, 1_500], Speed::RealTime),
            micros(&[1_000, 500, 0])
        );
    }

    #[test]
    fn times_divides_the_gaps_without_losing_sub_microsecond_precision() {
        let two = Speed::Times(NonZeroU32::new(2).unwrap());
        assert_eq!(
            waits(&[0, 1_500, 1_501], two),
            vec![
                Duration::from_micros(750),
                Duration::from_nanos(500),
                Duration::ZERO,
            ]
        );
        let thousand = Speed::Times(NonZeroU32::new(1_000).unwrap());
        assert_eq!(
            waits(&[0, 999], thousand),
            vec![Duration::from_nanos(999), Duration::ZERO,]
        );
    }

    #[test]
    fn max_never_waits() {
        assert_eq!(waits(&[0, 1_000, 5_000], Speed::Max), micros(&[0, 0, 0]));
    }

    #[test]
    fn backward_and_equal_timestamps_wait_zero() {
        assert_eq!(
            waits(&[100, 40, 40, 90], Speed::RealTime),
            micros(&[0, 0, 0, 0])
        );
    }

    #[test]
    fn a_reordered_timestamp_does_not_stretch_the_gap_after_it() {
        assert_eq!(
            waits(&[1_000_000, 0, 1_000_010], Speed::RealTime),
            micros(&[0, 10, 0])
        );
    }

    #[test]
    fn input_is_read_lazily() {
        let endless = iter::successors(Some(0), |ts| Some(ts + 1))
            .zip(1..)
            .map(|(ts, order_id)| (ts, Command::Cancel { order_id }));
        assert_eq!(pace(endless, Speed::RealTime).take(3).count(), 3);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert_eq!(pace(Vec::new(), Speed::RealTime).count(), 0);
    }

    #[test]
    fn commands_come_out_unchanged_and_in_order() {
        let input = flow(&[5, 6, 7]);
        let out: Vec<(Ts, Command)> = pace(input.clone(), Speed::Max)
            .map(|s| (s.ts, s.command))
            .collect();
        assert_eq!(out, input);
    }

    const ACK: &str = r#"{"channel":"subscriptionResponse","data":{"method":"subscribe"}}"#;
    const PONG: &str = r#"{"channel":"pong"}"#;
    // A trade from before the first snapshot, as in the backlog sent on subscribing.
    const BACKLOG: &str = r#"{"channel":"trades","data":[{"coin":"BTC","side":"A","px":"83990.0","sz":"0.1","time":900,"tid":1}]}"#;
    const FIRST: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1000,"levels":[[{"n":3,"px":"84000.0","sz":"0.5"},{"n":3,"px":"83999.0","sz":"1.0"},{"n":1,"px":"83998.0","sz":"1.0"},{"n":1,"px":"83997.0","sz":"1.0"},{"n":1,"px":"83996.0","sz":"1.0"},{"n":1,"px":"83995.0","sz":"1.0"}],[{"n":3,"px":"84001.0","sz":"0.5"},{"n":3,"px":"84002.0","sz":"1.0"}]]}}"#;
    const SECOND: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":2000,"levels":[[{"n":3,"px":"84000.0","sz":"0.5"},{"n":3,"px":"83999.0","sz":"1.0"},{"n":1,"px":"83998.0","sz":"1.0"},{"n":1,"px":"83997.0","sz":"1.0"},{"n":1,"px":"83996.0","sz":"1.0"},{"n":1,"px":"83995.0","sz":"1.0"}],[{"n":3,"px":"84001.0","sz":"0.3"},{"n":3,"px":"84002.0","sz":"1.0"}]]}}"#;
    // At the second snapshot's time, so the snapshot already reflects it.
    const TRADE: &str = r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"84001.0","sz":"0.2","time":2000,"tid":2}]}"#;

    /// The trade is written after the snapshot that reflects it; line 4 is empty.
    /// A finished recording: like the recorder's output, every line ends with
    /// a line break.
    fn recording(extra: &[&str]) -> String {
        let mut lines = vec![ACK, BACKLOG, FIRST, "", PONG, SECOND, TRADE];
        lines.extend(extra);
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn skipped_channels_make_no_commands_and_a_trade_precedes_its_snapshot() {
        let loaded = load(&recording(&[]), &engine::market()).unwrap();
        let ts: Vec<Ts> = loaded.commands.iter().map(|(ts, _)| *ts).collect();
        assert_eq!(
            ts,
            [1_000_000; 8]
                .into_iter()
                .chain([2_000_000])
                .collect::<Vec<_>>()
        );
        assert!(matches!(
            loaded.commands[8].1,
            Command::Place {
                order_type: protocol::OrderType::Limit {
                    time_in_force: protocol::TimeInForce::Ioc,
                    ..
                },
                ..
            }
        ));
        // Had the snapshot gone first, the shrunk ask level would need a replace.
        assert_eq!(loaded.divergence, Divergence::default());
    }

    #[test]
    fn trades_at_or_before_the_first_snapshot_are_dropped_and_counted() {
        let loaded = load(&recording(&[]), &engine::market()).unwrap();
        assert_eq!(loaded.dropped_trades, 1);
        let at_snapshot = TRADE.replace("2000", "1000");
        let loaded = load(&[FIRST, &at_snapshot].join("\n"), &engine::market()).unwrap();
        assert_eq!(loaded.dropped_trades, 1);
        assert_eq!(loaded.commands.len(), 8);
    }

    /// A snapshot with `bids` bid levels and one ask, at `time` milliseconds.
    fn book_line(time: u64, bids: u64) -> String {
        let levels: Vec<String> = (0..bids)
            .map(|i| format!(r#"{{"n":1,"px":"{}.0","sz":"1.0"}}"#, 84_000 - i))
            .collect();
        let ask = r#"{"n":1,"px":"84001.0","sz":"1.0"}"#;
        format!(
            r#"{{"channel":"l2Book","data":{{"coin":"BTC","time":{time},"levels":[[{}],[{ask}]]}}}}"#,
            levels.join(",")
        )
    }

    #[test]
    fn snapshots_are_classified_by_depth() {
        let market = engine::market();
        // One full snapshot, then a top one: it moves the ask only inside its window.
        let full = book_line(1_000, TOP_DEPTH as u64 + 1);
        let top = book_line(2_000, TOP_DEPTH as u64).replace("84001.0", "84002.0");
        let loaded = load(&[full.clone(), top].join("\n"), &market).unwrap();
        // 6 bids and 1 ask, then the ask replaced and the 5 bids unchanged.
        assert_eq!(loaded.commands.len(), 7 + 2);
        assert_eq!(loaded.dropped_top_snapshots, 0);
        // A snapshot of exactly TOP_DEPTH levels is top-only and cannot start a replay.
        let only_top = book_line(1_000, TOP_DEPTH as u64);
        assert!(matches!(
            load(&only_top, &market),
            Err(LoadError::NoSnapshot)
        ));
        // One level more is a full snapshot.
        assert!(load(&full, &market).is_ok());
    }

    #[test]
    fn top_snapshots_before_the_first_full_one_are_dropped_and_counted() {
        let top = |time| book_line(time, TOP_DEPTH as u64);
        let full = book_line(3_000, TOP_DEPTH as u64 + 1);
        let text = [top(1_000), top(2_000), full.clone(), top(4_000)].join("\n");
        let loaded = load(&text, &engine::market()).unwrap();
        assert_eq!(loaded.dropped_top_snapshots, 2);
        assert_eq!(loaded.commands.iter().map(|c| c.0).min(), Some(3_000_000));
        let from_full = load(&[full, top(4_000)].join("\n"), &engine::market()).unwrap();
        assert_eq!(loaded.commands, from_full.commands);
    }

    /// A best bid and offer at `time` milliseconds; `None` is a `null` side.
    fn bbo_line(time: u64, bid: Option<&str>, ask: Option<&str>) -> String {
        let side = |px: Option<&str>| {
            px.map_or("null".to_owned(), |px| {
                format!(r#"{{"n":1,"px":"{px}","sz":"1.0"}}"#)
            })
        };
        format!(
            r#"{{"channel":"bbo","data":{{"coin":"BTC","time":{time},"bbo":[{},{}]}}}}"#,
            side(bid),
            side(ask)
        )
    }

    #[test]
    fn a_trade_at_the_time_of_a_best_bid_and_offer_comes_first() {
        let full = book_line(1_000, TOP_DEPTH as u64 + 1);
        // The best bid and offer is written first and restores the level the
        // trade takes 0.2 from, so it only has work to do after the trade.
        let bbo = bbo_line(2_000, Some("84000.0"), Some("84001.0"));
        let loaded = load(&[full, bbo, TRADE.to_owned()].join("\n"), &engine::market()).unwrap();
        assert_eq!(loaded.commands.len(), 7 + 2);
        assert!(matches!(
            loaded.commands[7].1,
            Command::Place {
                order_type: protocol::OrderType::Limit {
                    time_in_force: protocol::TimeInForce::Ioc,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            loaded.commands[8].1,
            Command::Place {
                qty: 20_000,
                order_type: protocol::OrderType::Limit {
                    time_in_force: protocol::TimeInForce::PostOnly,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn best_bid_and_offers_before_the_first_full_snapshot_are_dropped_and_counted() {
        let bbo = |time| bbo_line(time, Some("84000.0"), Some("84001.0"));
        let full = book_line(3_000, TOP_DEPTH as u64 + 1);
        let loaded = load(
            &[bbo(1_000), full.clone(), bbo(4_000)].join("\n"),
            &engine::market(),
        )
        .unwrap();
        assert_eq!((loaded.dropped_top_snapshots, loaded.skipped_bbos), (1, 0));
        let from_full = load(&[full, bbo(4_000)].join("\n"), &engine::market()).unwrap();
        assert_eq!(loaded.commands, from_full.commands);
    }

    #[test]
    fn a_locked_or_crossed_best_bid_and_offer_is_skipped_and_counted() {
        let full = book_line(1_000, TOP_DEPTH as u64 + 1);
        let lines = [
            full.clone(),
            bbo_line(2_000, Some("84001.0"), Some("84001.0")),
            bbo_line(3_000, Some("84002.0"), Some("84001.0")),
        ];
        let loaded = load(&lines.join("\n"), &engine::market()).unwrap();
        assert_eq!((loaded.dropped_top_snapshots, loaded.skipped_bbos), (0, 2));
        assert_eq!(
            loaded.commands,
            load(&full, &engine::market()).unwrap().commands
        );
        // Other conversion failures still fail the load, with the line.
        let eth = bbo_line(2_000, Some("84000.0"), None).replace("BTC", "ETH");
        let err = load(&[full, eth].join("\n"), &engine::market()).unwrap_err();
        assert!(matches!(err, LoadError::Convert { line: 2, .. }), "{err}");
    }

    #[test]
    fn a_recording_without_a_snapshot_is_rejected() {
        for text in ["", ACK, &[ACK, PONG, TRADE].join("\n")] {
            let err = load(text, &engine::market()).unwrap_err();
            assert!(matches!(err, LoadError::NoSnapshot), "{text:?}: {err}");
        }
    }

    #[test]
    fn an_unknown_or_missing_channel_names_its_line() {
        let error = r#"{"channel":"error","data":"x"}"#;
        let err = load(&recording(&[error]), &engine::market()).unwrap_err();
        assert!(matches!(&err, LoadError::Channel { line: 8, channel } if channel == "error"));
        assert!(err.to_string().starts_with("line 8:"));
        let err = load(&recording(&["{}"]), &engine::market()).unwrap_err();
        assert!(matches!(&err, LoadError::Channel { line: 8, channel } if channel == "(none)"));
        // The adapter parses `l2` lines, but replays do not use them yet.
        let l2 = r#"{"channel":"l2","data":{"c":"abc"}}"#;
        let err = load(&recording(&[l2]), &engine::market()).unwrap_err();
        assert!(matches!(&err, LoadError::Channel { line: 8, channel } if channel == "l2"));
    }

    #[test]
    fn a_malformed_line_names_its_line() {
        assert!(matches!(
            load(&recording(&["not json"]), &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));
        let bad_data = r#"{"channel":"trades","data":"x"}"#;
        assert!(matches!(
            load(bad_data, &engine::market()),
            Err(LoadError::Parse { line: 1, .. })
        ));
    }

    #[test]
    fn repeated_keys_in_a_line_are_rejected() {
        let level = r#"{"n":3,"px":"84000.0","px":"90000.0","sz":"0.5"}"#;
        let doubled = FIRST.replacen(r#"{"n":3,"px":"84000.0","sz":"0.5"}"#, level, 1);
        assert_ne!(doubled, FIRST);
        assert!(matches!(
            load(&recording(&[&doubled]), &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));
        let channel = format!(r#"{{"channel":"pong",{}"#, &SECOND[1..]);
        assert!(matches!(
            load(&recording(&[&channel]), &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));
        let channel = format!(r#"{},"channel":"pong"}}"#, &SECOND[..SECOND.len() - 1]);
        assert!(matches!(
            load(&recording(&[&channel]), &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));
        let trade = TRADE.replacen(r#""px":"84001.0","#, r#""px":"84001.0","px":"1.0","#, 1);
        assert_ne!(trade, TRADE);
        let data = r#"{"channel":"trades","data":[],"data":[]}"#;
        for text in [trade.as_str(), data] {
            assert!(matches!(
                load(&recording(&[text]), &engine::market()),
                Err(LoadError::Parse { line: 8, .. })
            ));
        }
    }

    #[test]
    fn a_line_that_is_not_an_object_is_rejected() {
        let market = engine::market();
        assert!(matches!(
            load(&recording(&[r#"["pong"]"#]), &market),
            Err(LoadError::Channel { line: 8, .. })
        ));
        for text in ["42", r#"{"channel":5}"#] {
            assert!(matches!(
                load(&recording(&[text]), &market),
                Err(LoadError::Parse { line: 8, .. })
            ));
        }
    }

    #[test]
    fn a_cut_off_last_line_is_left_out_while_the_file_is_unfinished() {
        let complete = load(&recording(&[]), &engine::market()).unwrap();
        assert!(!complete.incomplete_last_line);

        let unfinished = recording(&[]) + &SECOND[..40];
        let loaded = load(&unfinished, &engine::market()).unwrap();
        assert!(loaded.incomplete_last_line);
        assert_eq!(loaded.commands, complete.commands);

        // A finished file, or a cut-off line followed by more lines, is still
        // an error.
        let finished = format!("{unfinished}\n");
        assert!(matches!(
            load(&finished, &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));
        let in_the_middle = format!("{unfinished}\n{PONG}");
        assert!(matches!(
            load(&in_the_middle, &engine::market()),
            Err(LoadError::Parse { line: 8, .. })
        ));

        // A last line without a line break that parses is used as usual.
        let no_break = recording(&[]).trim_end().to_owned();
        let loaded = load(&no_break, &engine::market()).unwrap();
        assert!(!loaded.incomplete_last_line);
        assert_eq!(loaded.commands, complete.commands);
    }

    #[test]
    fn an_unconvertible_value_names_its_line() {
        let text = recording(&[]).replace("84001.0", "abc");
        assert!(matches!(
            load(&text, &engine::market()),
            Err(LoadError::Convert { line: 3, .. })
        ));
    }

    #[test]
    fn long_waits_report_recorded_gaps_above_the_limit() {
        let minute = 60_000_000;
        let commands = flow(&[0, minute + 1, minute + 2, 3 * minute]);
        assert_eq!(
            long_waits(&commands, Duration::from_secs(60)),
            vec![
                (0, Duration::from_micros(minute + 1)),
                (2, Duration::from_micros(2 * minute - 2)),
            ]
        );
    }

    #[test]
    fn a_gap_equal_to_the_limit_is_not_long() {
        let minute = 60_000_000;
        let commands = flow(&[0, minute, 2 * minute + 1]);
        assert_eq!(
            long_waits(&commands, Duration::from_secs(60)),
            vec![(1, Duration::from_micros(minute + 1))]
        );
    }

    #[test]
    fn speeds_parse_from_text() {
        let times = |n| Speed::Times(NonZeroU32::new(n).unwrap());
        assert_eq!("max".parse(), Ok(Speed::Max));
        assert_eq!("1".parse(), Ok(Speed::RealTime));
        assert_eq!("10".parse(), Ok(times(10)));
        assert_eq!("step".parse(), Ok(Speed::Step));
        for bad in ["0", "-1", "1.5", "MAX", "", "Step", "steps"] {
            assert_eq!(bad.parse::<Speed>(), Err(InvalidSpeed), "{bad:?}");
        }
    }

    /// Consumes a held flow of three steps on a thread and reports what it saw
    /// once the flow ends; `release` is then used to end the hold.
    fn assert_released_by(release: impl FnOnce(std::sync::mpsc::Sender<()>)) {
        let (sender, wait) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let steps = pace(flow(&[5, 6, 7]), Speed::Max);
        let consumer = std::thread::spawn(move || {
            let seen: Vec<Ts> = hold(steps, wait).map(|step| step.ts).collect();
            done.send(seen).unwrap();
        });
        assert_eq!(
            finished.recv_timeout(Duration::from_millis(200)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout),
            "the flow must not end while the release sender is alive"
        );
        release(sender);
        assert_eq!(
            finished.recv_timeout(Duration::from_secs(5)).unwrap(),
            [5, 6, 7]
        );
        consumer.join().unwrap();
    }

    #[test]
    fn hold_passes_steps_through_and_ends_when_the_sender_is_dropped() {
        assert_released_by(drop);
    }

    #[test]
    fn hold_ends_when_a_message_is_sent() {
        assert_released_by(|sender| sender.send(()).unwrap());
    }

    type Report = Option<(Ts, Duration)>;

    /// Runs `step_on` over commands at ts 10, 10, 10, 20, 20 on a thread that
    /// reports each step as it arrives. After the flow's first `None` it asks
    /// once more, to show that an ended flow stays ended, and then exits.
    fn stepping() -> (
        std::sync::mpsc::Sender<Signal>,
        std::sync::mpsc::Receiver<Report>,
        std::thread::JoinHandle<()>,
    ) {
        let (signals, receive) = std::sync::mpsc::channel();
        let (seen, reports) = std::sync::mpsc::channel();
        // A real-time pace, so the zero waits come from `step_on`.
        let steps = pace(flow(&[10, 10, 10, 20, 20]), Speed::RealTime);
        let consumer = std::thread::spawn(move || {
            let mut steps = step_on(steps, receive);
            let mut next = || steps.next().map(|s| (s.ts, s.wait));
            loop {
                let report = next();
                let ended = report.is_none();
                if seen.send(report).is_err() {
                    return;
                }
                if ended {
                    let _ = seen.send(next());
                    return;
                }
            }
        });
        (signals, reports, consumer)
    }

    fn drain(reports: &std::sync::mpsc::Receiver<Report>, count: usize) -> Vec<Report> {
        (0..count)
            .map(|_| reports.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect()
    }

    fn quiet(reports: &std::sync::mpsc::Receiver<Report>) {
        assert!(reports.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn each_next_releases_one_time_group_with_zero_waits() {
        let (signals, reports, consumer) = stepping();
        quiet(&reports);
        signals.send(Signal::Next).unwrap();
        let zero = Duration::ZERO;
        assert_eq!(drain(&reports, 3), vec![Some((10, zero)); 3]);
        quiet(&reports);
        signals.send(Signal::Next).unwrap();
        assert_eq!(drain(&reports, 2), vec![Some((20, zero)); 2]);
        // The end of the recording needs no further signal.
        assert_eq!(drain(&reports, 2), [None, None]);
        drop(signals);
        consumer.join().unwrap();
    }

    #[test]
    fn stop_ends_the_flow_for_good_before_or_between_groups() {
        for groups_before in [0, 1] {
            let (signals, reports, consumer) = stepping();
            for _ in 0..groups_before {
                signals.send(Signal::Next).unwrap();
                assert_eq!(drain(&reports, 3).len(), 3);
            }
            signals.send(Signal::Stop).unwrap();
            // The sender is still alive: the flow must end without it.
            assert_eq!(drain(&reports, 2), [None, None]);
            drop(signals);
            consumer.join().unwrap();
        }
    }

    #[test]
    fn a_dropped_sender_ends_the_flow_without_releasing() {
        let (signals, reports, consumer) = stepping();
        quiet(&reports);
        drop(signals);
        assert_eq!(drain(&reports, 2), [None, None]);
        consumer.join().unwrap();
    }
}
