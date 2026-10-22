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
}

/// The text is neither `max` nor a positive integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("speed must be `max` or a positive integer")]
pub struct InvalidSpeed;

impl FromStr for Speed {
    type Err = InvalidSpeed;

    /// `max`, or a positive integer factor, where `1` is real time.
    fn from_str(text: &str) -> Result<Self, InvalidSpeed> {
        if text == "max" {
            return Ok(Self::Max);
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
            Speed::Max => Duration::ZERO,
        };
        Some(Step { command, ts, wait })
    })
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
        info!("replay finished, holding the final book");
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
    /// The recording holds no book snapshot, so there is nothing to serve.
    #[error("the recording holds no l2Book snapshot")]
    NoSnapshot,
}

/// A recording translated into commands.
#[derive(Debug)]
pub struct Loaded {
    /// Commands with their venue times, in time order.
    pub commands: Vec<(Ts, Command)>,
    /// How far the translator's book is known to differ from the recorded one.
    pub divergence: Divergence,
    /// Trades at or before the first snapshot's time, which were left out.
    pub dropped_trades: usize,
}

/// A recorded book or trade, ready to be translated.
enum Recorded {
    Trade(Trade),
    Snapshot(BookSnapshot),
}

impl Recorded {
    /// Sort key: by time, trades first, because a snapshot already reflects
    /// the trades up to its own time.
    fn key(&self) -> (Ts, u8) {
        match self {
            Self::Trade(trade) => (trade.ts, 0),
            Self::Snapshot(snapshot) => (snapshot.ts, 1),
        }
    }
}

/// Translates a JSON Lines recording of `l2Book` and `trades` messages, with
/// prices and sizes converted for `market`.
///
/// Subscription acknowledgements and `pong` replies are skipped, as are empty
/// lines. Recorded order can differ from time order, so the data is sorted by
/// time (stable) before it is translated. Trades at or before the first
/// snapshot's time are dropped: the first trades message after subscribing is
/// a backlog, and a snapshot already reflects every trade up to its time.
///
/// # Errors
///
/// Returns a [`LoadError`] for the first line that is malformed, on another
/// channel, or not convertible, or if the recording has no snapshot.
pub fn load(recording: &str, market: &MarketInfo) -> Result<Loaded, LoadError> {
    let mut recorded = Vec::new();
    for (index, text) in recording.lines().enumerate() {
        let line = index + 1;
        if text.trim().is_empty() {
            continue;
        }
        let parse = |source| LoadError::Parse { line, source };
        let value: serde_json::Value = serde_json::from_str(text).map_err(parse)?;
        let channel = value.get("channel").and_then(|c| c.as_str());
        if matches!(channel, Some("subscriptionResponse" | "pong")) {
            continue;
        }
        if !matches!(channel, Some("l2Book" | "trades")) {
            let channel = channel.unwrap_or("(none)").to_owned();
            return Err(LoadError::Channel { line, channel });
        }
        let convert = |source| LoadError::Convert { line, source };
        match serde_json::from_value(value).map_err(parse)? {
            Message::L2Book(book) => {
                recorded.push(Recorded::Snapshot(
                    book.to_snapshot(market).map_err(convert)?,
                ));
            }
            Message::Trades(trades) => {
                for trade in &trades {
                    recorded.push(Recorded::Trade(trade.to_trade(market).map_err(convert)?));
                }
            }
        }
    }
    recorded.sort_by_key(Recorded::key);
    let first_snapshot = recorded
        .iter()
        .find_map(|item| match item {
            Recorded::Snapshot(snapshot) => Some(snapshot.ts),
            Recorded::Trade(_) => None,
        })
        .ok_or(LoadError::NoSnapshot)?;
    let mut translator = Translator::new();
    let mut commands = Vec::new();
    let mut dropped_trades = 0;
    for item in &recorded {
        match item {
            Recorded::Trade(trade) if trade.ts <= first_snapshot => dropped_trades += 1,
            Recorded::Trade(trade) => commands.extend(translator.on_trade(trade)),
            Recorded::Snapshot(snapshot) => commands.extend(translator.on_snapshot(snapshot)),
        }
    }
    Ok(Loaded {
        commands,
        divergence: translator.divergence(),
        dropped_trades,
    })
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
    const FIRST: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":1000,"levels":[[{"n":3,"px":"84000.0","sz":"0.5"},{"n":3,"px":"83999.0","sz":"1.0"}],[{"n":3,"px":"84001.0","sz":"0.5"},{"n":3,"px":"84002.0","sz":"1.0"}]]}}"#;
    const SECOND: &str = r#"{"channel":"l2Book","data":{"coin":"BTC","time":2000,"levels":[[{"n":3,"px":"84000.0","sz":"0.5"},{"n":3,"px":"83999.0","sz":"1.0"}],[{"n":3,"px":"84001.0","sz":"0.3"},{"n":3,"px":"84002.0","sz":"1.0"}]]}}"#;
    // At the second snapshot's time, so the snapshot already reflects it.
    const TRADE: &str = r#"{"channel":"trades","data":[{"coin":"BTC","side":"B","px":"84001.0","sz":"0.2","time":2000,"tid":2}]}"#;

    /// The trade is written after the snapshot that reflects it; line 4 is empty.
    fn recording(extra: &[&str]) -> String {
        let mut lines = vec![ACK, BACKLOG, FIRST, "", PONG, SECOND, TRADE];
        lines.extend(extra);
        lines.join("\n")
    }

    #[test]
    fn skipped_channels_make_no_commands_and_a_trade_precedes_its_snapshot() {
        let loaded = load(&recording(&[]), &engine::market()).unwrap();
        let ts: Vec<Ts> = loaded.commands.iter().map(|(ts, _)| *ts).collect();
        assert_eq!(ts, [1_000_000, 1_000_000, 1_000_000, 1_000_000, 2_000_000]);
        assert!(matches!(
            loaded.commands[4].1,
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
        assert_eq!(loaded.commands.len(), 4);
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
        for bad in ["0", "-1", "1.5", "MAX", ""] {
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
}
