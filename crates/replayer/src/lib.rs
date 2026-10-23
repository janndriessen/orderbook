//! Loads a recorded Hyperliquid session as commands and paces them into steps
//! for the service.

use std::collections::BTreeSet;
use std::iter;
use std::num::NonZeroU32;
use std::str::FromStr;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use engine::Step;
use hyperliquid_adapter::{
    BookSnapshot, ConvertError, Divergence, Grouping, L2Error, MergeError, Message, Trade,
    Translator, WireL2,
};
use protocol::{Command, MarketInfo, Ts};
use tracing::info;

mod l2_flow;

pub use l2_flow::{L2Flow, L2Stats};

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

/// Why a recording could not be loaded. A recording is never cut short: a bad
/// line fails the whole load.
///
/// Errors come out in line order, except that those of an `l2` line that
/// parses and names a grouping but does not apply to its grouping's book
/// ([`LoadError::L2`], [`LoadError::Backwards`]) come out after the lines
/// before them have all been read: a bad line anywhere else in the recording
/// is reported first. [`LoadError::NoSnapshot`] comes last.
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
    /// An `l2` line has no `subscription`, or one that is not a BTC price
    /// grouping the source offers.
    #[error("line {line}: l2 line without a valid subscription")]
    Subscription {
        /// 1-based line number.
        line: usize,
    },
    /// An `l2` line could not be applied to its grouping's book.
    #[error("line {line}: {source}")]
    L2 {
        /// 1-based line number.
        line: usize,
        /// Why the decoder refused it.
        source: L2Error,
    },
    /// One grouping's books go back in time behind a push that was already
    /// replayed: its book is at or before that push's time although the
    /// grouping has sent a later one.
    #[error("line {line}: l2 book older than a push already replayed")]
    Backwards {
        /// 1-based line number.
        line: usize,
    },
    /// The recording holds no full book to start from.
    #[error("the recording holds no full book snapshot")]
    NoSnapshot,
}

/// A recording translated into commands.
#[derive(Debug, Default)]
pub struct Loaded {
    /// Commands with their venue times, in time order.
    pub commands: Vec<(Ts, Command)>,
    /// How far the translator's book is known to differ from the recorded one.
    pub divergence: Divergence,
    /// Trades at or before the first full snapshot's time, which were left out.
    pub dropped_trades: usize,
    /// Trades after the last complete push that merged in the `l2` feed,
    /// which were left out because no push comes to release them. Always 0
    /// without `l2` lines.
    pub dropped_late_trades: usize,
    /// Trades that were written after the push they belong to had already
    /// been replayed, and were held to the next push instead. Always 0
    /// without `l2` lines.
    pub late_trades: usize,
    /// Top-of-book snapshots, best bid and offer messages included, before
    /// the first full snapshot, which were left out.
    pub dropped_top_snapshots: usize,
    /// Best bid and offer messages with a bid at or above the ask, which were
    /// left out as unusable.
    pub skipped_bbos: usize,
    /// Whether the last line was cut off and left out, as happens while the
    /// recorder is still writing.
    pub incomplete_last_line: bool,
    /// Lines of the documented book feeds (`l2Book` and `bbo`) left out
    /// because the recording has an `l2` feed, which shows the book at every
    /// price grouping. They are not read, so a bad one is not noticed.
    pub skipped_book_lines: usize,
    /// Push times of the `l2` feed before the one at which the last grouping
    /// sent its first book, which were left out: while the connections open, some groupings
    /// are bound to be missing.
    pub startup_pushes: usize,
    /// Push times of the `l2` feed, from the one at which the last grouping
    /// sent its first book on, at which some grouping sent no book although
    /// it sent a later one, which were left out. The book stays as it was for
    /// as long as the pushes stay incomplete.
    pub skipped_pushes: usize,
    /// Push times of the `l2` feed at the end of the recording at which some
    /// grouping sent no book and none later, which were left out: the
    /// recording ended, or a grouping stopped sending, before they could
    /// complete.
    pub trailing_pushes: usize,
    /// Complete pushes whose groupings could not be merged, which were left
    /// out like incomplete ones.
    pub failed_merges: usize,
    /// The time of the first push that failed to merge, and why.
    pub first_merge_error: Option<(Ts, MergeError)>,
    /// How far the merged books missed their inputs, in lots, summed over
    /// the complete pushes (see [`hyperliquid_adapter::Merged`]).
    pub conflicting_qty: u64,
    /// `error` lines, which the source sends for a request it refused, left
    /// out.
    pub skipped_error_lines: usize,
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

/// What a line holds, from its header alone.
enum Feed {
    /// A line that cannot be loaded; the error is raised when its turn comes.
    Failed(LoadError),
    L2Book,
    Bbo,
    Trades,
    /// An `l2` line and the grouping its `subscription` names.
    L2(Grouping),
}

/// A line that holds data.
struct Line<'a> {
    number: usize,
    text: &'a str,
    feed: Feed,
}

/// The lines of a recording that hold data, and what was left out on the way.
struct Scan<'a> {
    lines: Vec<Line<'a>>,
    incomplete_last_line: bool,
    skipped_error_lines: usize,
}

/// The `subscription` the recorder adds to every `l2` line, read apart from
/// the message because the adapter's message type does not carry it. `c` is
/// the coin, `s` the significant figures and `m` the mantissa, absent for 1.
/// Without `s` (absent or null) the subscription names no rounding: the
/// grouping of single ticks.
#[derive(serde::Deserialize)]
struct SubscribedLine {
    subscription: Subscription,
}

#[derive(serde::Deserialize)]
struct Subscription {
    c: String,
    s: Option<u32>,
    m: Option<u32>,
}

/// The grouping an `l2` line is for, if its `subscription` names a BTC
/// grouping the source offers.
fn grouping_of(text: &str) -> Option<Grouping> {
    let SubscribedLine {
        subscription: Subscription { c, s, m },
    } = serde_json::from_str(text).ok()?;
    let grouping = Grouping {
        sig_figs: s,
        mantissa: m.unwrap_or(1),
    };
    (c == "BTC" && grouping.is_offered()).then_some(grouping)
}

/// What a line is, as far as its header can tell.
enum Classified {
    /// Blank, or a subscription acknowledgement or `pong`: nothing to load.
    Skip,
    /// The last line of a recording still being written, cut off.
    Cut,
    /// An `error` line, which the source sends for a request it refused.
    ErrorLine,
    /// A line that holds data, or fails to load.
    Feed(Feed),
}

/// Reads one line's header. `may_be_cut` is whether the line may be the cut-off
/// end of a recording, which is left out when it does not parse.
fn classify(number: usize, text: &str, may_be_cut: bool) -> Classified {
    if text.trim().is_empty() {
        return Classified::Skip;
    }
    let failed = |error| Classified::Feed(Feed::Failed(error));
    let channel = match serde_json::from_str::<LineHeader>(text) {
        Ok(_) if !text.trim_start().starts_with('{') => {
            // The header struct also reads a JSON array, whose first
            // element would pass for a channel.
            let channel = "(none)".to_owned();
            return failed(LoadError::Channel {
                line: number,
                channel,
            });
        }
        Ok(header) => header.channel,
        Err(_) if may_be_cut => return Classified::Cut,
        Err(source) => {
            return failed(LoadError::Parse {
                line: number,
                source,
            });
        }
    };
    Classified::Feed(match channel.as_deref() {
        Some("subscriptionResponse" | "pong") => return Classified::Skip,
        Some("error") => return Classified::ErrorLine,
        Some("l2Book") => Feed::L2Book,
        Some("bbo") => Feed::Bbo,
        Some("trades") => Feed::Trades,
        Some("l2") => match grouping_of(text) {
            Some(grouping) => Feed::L2(grouping),
            None => Feed::Failed(LoadError::Subscription { line: number }),
        },
        other => {
            let channel = other.unwrap_or("(none)").to_owned();
            Feed::Failed(LoadError::Channel {
                line: number,
                channel,
            })
        }
    })
}

/// Reads every line's header, so the kind of recording is known before any
/// line is converted.
fn scan(recording: &str) -> Scan<'_> {
    let unfinished_line = (!recording.ends_with('\n')).then(|| recording.lines().count());
    let mut scan = Scan {
        lines: Vec::new(),
        incomplete_last_line: false,
        skipped_error_lines: 0,
    };
    for (index, text) in recording.lines().enumerate() {
        let number = index + 1;
        match classify(number, text, Some(number) == unfinished_line) {
            Classified::Skip => {}
            Classified::Cut => scan.incomplete_last_line = true,
            Classified::ErrorLine => scan.skipped_error_lines += 1,
            Classified::Feed(feed) => scan.lines.push(Line { number, text, feed }),
        }
    }
    scan
}

/// The line as a message of the adapter, for a line whose header named its
/// channel.
fn message(line: usize, text: &str) -> Result<Message, LoadError> {
    serde_json::from_str(text).map_err(|source| LoadError::Parse { line, source })
}

/// The line named a channel that its message turned out not to be on.
fn mismatch(line: usize) -> LoadError {
    let channel = "(mismatch)".to_owned();
    LoadError::Channel { line, channel }
}

/// Translates a JSON Lines recording of `l2`, `l2Book`, `bbo` and `trades`
/// messages, with prices and sizes converted for `market`.
///
/// The headers are read first: a recording with `l2` lines is replayed from
/// them alone, any other from the documented book feeds.
///
/// With `l2` lines (the recorder's price groupings, each line carrying its
/// `subscription`), one decoder per grouping keeps its book. A push is
/// complete when every grouping in the recording has a book at exactly that
/// time, and the groupings of a complete push are merged into one full
/// snapshot, whose disagreement between groupings is summed in
/// `conflicting_qty`. The first complete push whose groupings merge starts
/// the replay. A push where some grouping sent no book is left out, and
/// counted in `startup_pushes` if it is before the time at which the last
/// grouping sent its first book, in `trailing_pushes` if no grouping sent a
/// later book, and in `skipped_pushes` otherwise; a complete push whose
/// groupings do not merge is counted in `failed_merges`. The
/// `l2Book` and `bbo` lines are counted in `skipped_book_lines` without being
/// read. Trades are held: a trade is translated at the time of the first
/// complete push at or after its own time, before that push's snapshot, so
/// the book changes only at pushes, as in the source's web app. Trades at or
/// before the first push that merges are the backlog sent on subscribing and
/// are counted in `dropped_trades`; trades after the last one are dropped and
/// counted in `dropped_late_trades`. A trade written after the push it belongs
/// to was already replayed (lines come in the order they were written, and a
/// trade can trail its push) goes to the next push instead and is counted in
/// `late_trades`. The assembly is [`L2Flow`], which takes the lines one at a
/// time.
///
/// Lines arrive roughly but not strictly in time order, since each grouping
/// has its own connection. A push is therefore replayed once every grouping
/// has sent a book after it (any later book of a grouping settles that it
/// sends none for the push), and the rest at the end; pushes are held until
/// then, and a grouping that stops early keeps every later push held until
/// the end. A grouping whose books go back in time behind a push already
/// replayed is an error. The lines are read in order and the header, channel,
/// subscription, parsing and conversion errors come out in line order; the
/// errors of applying an `l2` line to its grouping's book (`L2`, `Backwards`)
/// come after them, as described on [`LoadError`].
///
/// Without `l2` lines, the two documented book feeds send messages of the same
/// shape, so the depth tells them apart: a snapshot with at most [`TOP_DEPTH`]
/// levels on each side is the top of the book and is mirrored only inside the
/// price window it spans; a deeper one is a full snapshot. A full snapshot of
/// a book that really has at most that many levels per side is ambiguous and
/// is treated as a top snapshot: stale levels beyond its window linger until
/// the next full snapshot, and a side that is empty in it keeps its stale
/// levels, which can make post-only orders on the other side be rejected
/// (counted in `rejected_makers`).
///
/// A `bbo` message is a top snapshot of one level per side and is mirrored the
/// same way and counts as one for dropping. One with a bid at or above the ask
/// is left out and counted in `skipped_bbos`.
///
/// Subscription acknowledgements and `pong` replies are skipped, as are empty
/// lines and `error` lines (counted in `skipped_error_lines`). Recorded order
/// can differ from time order, so the data is sorted by time (stable) before
/// it is translated. Trades at or before the first full snapshot's time are
/// dropped: the first trades message after subscribing is a backlog, and a
/// snapshot already reflects every trade up to its time. Top snapshots before
/// the first full one are dropped too, so the replay always starts from a full
/// book.
///
/// A recording that does not end with a line break may end in the middle of a
/// line, because the recorder writes in blocks while it runs; such a last line
/// is left out when it does not parse.
///
/// # Errors
///
/// Returns a [`LoadError`] for the first line that is malformed, on another
/// channel, not convertible, or an `l2` line that does not apply; or if the
/// recording has no full snapshot.
pub fn load(recording: &str, market: &MarketInfo) -> Result<Loaded, LoadError> {
    let scan = scan(recording);
    if scan
        .lines
        .iter()
        .any(|line| matches!(line.feed, Feed::L2(_)))
    {
        load_l2(scan, market)
    } else {
        load_documented(scan, market)
    }
}

/// Loads a recording from the documented book feeds.
fn load_documented(scan: Scan<'_>, market: &MarketInfo) -> Result<Loaded, LoadError> {
    let mut skipped_bbos = 0;
    let mut recorded = Vec::new();
    for Line {
        number: line,
        text,
        feed,
    } in scan.lines
    {
        let convert = |source| LoadError::Convert { line, source };
        match (feed, message(line, text)) {
            (Feed::Failed(error), _) => return Err(error),
            (Feed::L2Book, Ok(Message::L2Book(book))) => {
                let snapshot = book.to_snapshot(market).map_err(convert)?;
                recorded.push(
                    if snapshot.bids.len().max(snapshot.asks.len()) <= TOP_DEPTH {
                        Recorded::Top(snapshot)
                    } else {
                        Recorded::Full(snapshot)
                    },
                );
            }
            (Feed::Bbo, Ok(Message::Bbo(bbo))) => match bbo.to_snapshot(market) {
                Ok(snapshot) => recorded.push(Recorded::Top(snapshot)),
                Err(ConvertError::LockedOrCrossed) => skipped_bbos += 1,
                Err(source) => return Err(convert(source)),
            },
            (Feed::Trades, Ok(Message::Trades(trades))) => {
                for trade in &trades {
                    recorded.push(Recorded::Trade(trade.to_trade(market).map_err(convert)?));
                }
            }
            (_, Err(error)) => return Err(error),
            _ => return Err(mismatch(line)),
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
        incomplete_last_line: scan.incomplete_last_line,
        skipped_error_lines: scan.skipped_error_lines,
        ..Loaded::default()
    })
}

/// Loads a recording from the `l2` feed.
fn load_l2(scan: Scan<'_>, market: &MarketInfo) -> Result<Loaded, LoadError> {
    let mut skipped_book_lines = 0;
    // Everything that can be wrong with a line on its own is found before any
    // book is applied, so those errors come out in line order.
    let mut items = Vec::new();
    let mut groupings = BTreeSet::new();
    for Line {
        number: line,
        text,
        feed,
    } in scan.lines
    {
        match feed {
            Feed::Failed(error) => return Err(error),
            Feed::L2Book | Feed::Bbo => skipped_book_lines += 1,
            Feed::Trades => {
                let Message::Trades(wires) = message(line, text)? else {
                    return Err(mismatch(line));
                };
                let mut trades = Vec::with_capacity(wires.len());
                for wire in &wires {
                    let convert = |source| LoadError::Convert { line, source };
                    trades.push(wire.to_trade(market).map_err(convert)?);
                }
                items.push(Item::Trades(trades));
            }
            Feed::L2(grouping) => {
                let Message::L2(wire) = message(line, text)? else {
                    return Err(mismatch(line));
                };
                groupings.insert(grouping);
                items.push(Item::Book(line, grouping, Box::new(wire)));
            }
        }
    }
    let mut flow = L2Flow::new(market.clone(), groupings.len());
    let mut commands = Vec::new();
    for item in items {
        match item {
            Item::Trades(trades) => flow.trades(trades),
            Item::Book(line, grouping, wire) => commands.extend(flow.book(line, grouping, &wire)?),
        }
    }
    let (rest, stats) = flow.finish();
    commands.extend(rest);
    if !stats.started {
        return Err(LoadError::NoSnapshot);
    }
    Ok(Loaded {
        divergence: stats.divergence,
        commands,
        dropped_trades: stats.dropped_trades,
        dropped_late_trades: stats.dropped_late_trades,
        late_trades: stats.late_trades,
        incomplete_last_line: scan.incomplete_last_line,
        skipped_book_lines,
        startup_pushes: stats.startup_pushes,
        skipped_pushes: stats.skipped_pushes,
        trailing_pushes: stats.trailing_pushes,
        failed_merges: stats.failed_merges,
        first_merge_error: stats.first_merge_error,
        conflicting_qty: stats.conflicting_qty,
        skipped_error_lines: scan.skipped_error_lines,
        ..Loaded::default()
    })
}

/// A line of an `l2` recording, read.
enum Item {
    Trades(Vec<Trade>),
    Book(usize, Grouping, Box<WireL2>),
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
    use std::collections::BTreeMap;

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
        let unknown = r#"{"channel":"orders","data":"x"}"#;
        let err = load(&recording(&[unknown]), &engine::market()).unwrap_err();
        assert!(matches!(&err, LoadError::Channel { line: 8, channel } if channel == "orders"));
        assert!(err.to_string().starts_with("line 8:"));
        let err = load(&recording(&["{}"]), &engine::market()).unwrap_err();
        assert!(matches!(&err, LoadError::Channel { line: 8, channel } if channel == "(none)"));
    }

    #[test]
    fn error_lines_are_skipped_and_counted() {
        let error = r#"{"channel":"error","data":"Already subscribed"}"#;
        let with = load(&recording(&[error, error]), &engine::market()).expect("loads");
        let without = load(&recording(&[]), &engine::market()).expect("loads");
        assert_eq!(with.skipped_error_lines, 2);
        assert_eq!(without.skipped_error_lines, 0);
        assert_eq!(with.commands, without.commands);
    }

    /// An `l2` line of one grouping, `s` significant figures and mantissa `m`
    /// (absent for 1), for coin `coin`, with a full book at `time` milliseconds.
    fn l2_for(
        coin: &str,
        s: u32,
        m: Option<u32>,
        time: u64,
        bids: &[(u64, &str)],
        asks: &[(u64, &str)],
    ) -> String {
        let side = |levels: &[(u64, &str)]| {
            let levels: Vec<String> = levels
                .iter()
                .map(|(px, sz)| format!(r#"{{"px":"{px}.0","sz":"{sz}","n":1}}"#))
                .collect();
            levels.join(",")
        };
        let m = m.map(|m| format!(r#""m":{m},"#)).unwrap_or_default();
        format!(
            r#"{{"channel":"l2","data":{{"s":{{"coin":"BTC","time":{time},"levels":[[{}],[{}]]}}}},"subscription":{{"c":"{coin}",{m}"s":{s},"type":"l2"}}}}"#,
            side(bids),
            side(asks)
        )
    }

    fn l2_line(
        s: u32,
        m: Option<u32>,
        time: u64,
        bids: &[(u64, &str)],
        asks: &[(u64, &str)],
    ) -> String {
        l2_for("BTC", s, m, time, bids, asks)
    }

    /// The $1 grouping at `time`, whose ask holds `ask` coins.
    fn fine(time: u64, ask: &str) -> String {
        l2_line(
            5,
            None,
            time,
            &[(81_000, "1.0"), (80_999, "2.0")],
            &[(81_001, ask)],
        )
    }

    /// The $10 grouping showing the same book as [`fine`].
    fn coarse(time: u64, ask: &str) -> String {
        l2_line(
            4,
            None,
            time,
            &[(81_000, "1.0"), (80_990, "2.0")],
            &[(81_010, ask)],
        )
    }

    /// Both groupings at `time`.
    fn both(time: u64, ask: &str) -> [String; 2] {
        [fine(time, ask), coarse(time, ask)]
    }

    fn trade_line(time: u64, side: &str, price: u64) -> String {
        format!(
            r#"{{"channel":"trades","data":[{{"coin":"BTC","side":"{side}","px":"{price}.0","sz":"0.2","time":{time},"tid":9}}]}}"#
        )
    }

    fn load_lines(lines: &[String]) -> Result<Loaded, LoadError> {
        let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
        load(&text, &engine::market())
    }

    type Levels = BTreeMap<i64, u64>;

    /// The bids and asks of a matcher after all commands, as price to total.
    fn book_after(loaded: &Loaded) -> (Levels, Levels) {
        let mut matcher = matching::Matcher::new();
        for (_, command) in &loaded.commands {
            matcher.apply(*command);
        }
        let levels = |side| {
            matcher
                .book()
                .levels(side)
                .map(|(price, level)| (price, level.total()))
                .collect()
        };
        (levels(protocol::Side::Buy), levels(protocol::Side::Sell))
    }

    fn times(loaded: &Loaded) -> Vec<Ts> {
        loaded.commands.iter().map(|(ts, _)| *ts).collect()
    }

    fn is_ioc(command: &Command) -> bool {
        matches!(
            command,
            Command::Place {
                order_type: protocol::OrderType::Limit {
                    time_in_force: protocol::TimeInForce::Ioc,
                    ..
                },
                ..
            }
        )
    }

    #[test]
    fn the_groupings_of_a_complete_push_are_merged_into_one_snapshot() {
        let lines = [both(1_000, "1.0"), both(2_000, "0.5")].concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.startup_pushes, 0);
        assert_eq!(loaded.skipped_pushes, 0);
        assert_eq!(loaded.failed_merges, 0);
        assert_eq!(loaded.conflicting_qty, 0);
        assert_eq!(loaded.skipped_book_lines, 0);
        assert_eq!(loaded.divergence, Divergence::default());
        // Three levels at the first push, one resized ask at the second.
        assert_eq!(times(&loaded)[..3], [1_000_000; 3]);
        assert!(times(&loaded)[3..].iter().all(|&ts| ts == 2_000_000));
        let (bids, asks) = book_after(&loaded);
        assert_eq!(bids, Levels::from([(81_000, 100_000), (80_999, 200_000)]));
        assert_eq!(asks, Levels::from([(81_001, 50_000)]));
    }

    #[test]
    fn explicit_mantissas_of_five_figures_are_groupings_too() {
        let lines = [
            fine(1_000, "1.0"),
            l2_line(
                5,
                Some(2),
                1_000,
                &[(81_000, "1.0"), (80_998, "2.0")],
                &[(81_002, "1.0")],
            ),
            l2_line(
                5,
                Some(5),
                1_000,
                &[(81_000, "1.0"), (80_995, "2.0")],
                &[(81_005, "1.0")],
            ),
        ];
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.failed_merges, 0);
        assert_eq!(loaded.conflicting_qty, 0);
        let (bids, asks) = book_after(&loaded);
        assert_eq!(bids, Levels::from([(81_000, 100_000), (80_999, 200_000)]));
        assert_eq!(asks, Levels::from([(81_001, 100_000)]));
    }

    #[test]
    fn pushes_before_every_grouping_has_started_are_start_up_pushes() {
        let lines = [vec![fine(500, "1.0")], both(1_000, "1.0").to_vec()].concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (1, 0));
        assert!(!times(&loaded).contains(&500_000));
    }

    #[test]
    fn the_push_of_a_groupings_first_book_is_not_a_start_up_push() {
        // The push at 1,000 is before the coarse grouping's first book at
        // 1,500; the push at 1,500 is the one that book is in.
        let lines = [
            vec![fine(1_000, "1.0"), coarse(1_500, "1.0")],
            both(2_000, "0.5").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (1, 1));
    }

    #[test]
    fn a_push_missing_a_grouping_after_start_up_is_skipped_and_counted() {
        let lines = [
            both(1_000, "1.0").to_vec(),
            // The coarse grouping comes half a second late.
            vec![fine(2_000, "0.5"), coarse(2_500, "0.5")],
            both(3_000, "0.4").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (0, 2));
        let seen = times(&loaded);
        assert!(!seen.contains(&2_000_000) && !seen.contains(&2_500_000));
        assert!(seen.contains(&3_000_000));
        let (_, asks) = book_after(&loaded);
        assert_eq!(asks, Levels::from([(81_001, 40_000)]));
    }

    #[test]
    fn conflicts_between_groupings_are_summed() {
        // The coarse bucket holds one lot less than the fine level inside it.
        let lines = [fine(1_000, "1.0"), coarse(1_000, "0.99999")];
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.conflicting_qty, 1);
    }

    #[test]
    fn a_push_that_does_not_merge_is_skipped_and_counted() {
        // 81,005 is not a bucket of the $10 grouping.
        let off_bucket = l2_line(4, None, 2_000, &[(81_005, "1.0")], &[(81_010, "1.0")]);
        let lines = [
            both(1_000, "1.0").to_vec(),
            vec![fine(2_000, "1.0"), off_bucket],
            both(3_000, "0.5").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.failed_merges, 1);
        assert!(matches!(
            loaded.first_merge_error,
            Some((2_000_000, MergeError::Bucket { .. }))
        ));
        assert!(!times(&loaded).contains(&2_000_000));
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 50_000)]));
    }

    #[test]
    fn a_trade_between_pushes_is_translated_at_the_next_push_before_its_snapshot() {
        let lines = |trade: Option<String>| {
            let mut lines = both(1_000, "1.0").to_vec();
            lines.extend(trade);
            lines.extend(both(2_000, "0.8"));
            lines
        };
        let without = load_lines(&lines(None)).expect("loads");
        let first = times(&without)
            .iter()
            .filter(|&&ts| ts == 1_000_000)
            .count();
        let loaded = load_lines(&lines(Some(trade_line(1_500, "B", 81_001)))).expect("loads");
        let seen = times(&loaded);
        assert!(!seen.contains(&1_500_000));
        assert!(seen[..first].iter().all(|&ts| ts == 1_000_000));
        assert!(seen[first..].iter().all(|&ts| ts == 2_000_000));
        assert!(is_ioc(&loaded.commands[first].1));
        // The snapshot after the trade finds the ask already at 0.8.
        assert_eq!(loaded.divergence, Divergence::default());
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 80_000)]));
    }

    /// Feeds `lines` to a flow of two groupings one at a time and returns the
    /// commands each line made, then those of finishing, then the counts.
    fn flow_run(lines: &[String]) -> (Vec<Vec<(Ts, Command)>>, L2Stats) {
        let mut flow = L2Flow::new(engine::market(), 2);
        let mut per_line = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            per_line.push(flow.line(index + 1, line).expect("accepts the line"));
        }
        let (rest, stats) = flow.finish();
        per_line.push(rest);
        (per_line, stats)
    }

    fn after(commands: Vec<(Ts, Command)>) -> (Levels, Levels) {
        book_after(&Loaded {
            commands,
            ..Loaded::default()
        })
    }

    #[test]
    fn a_flow_emits_a_push_once_every_grouping_has_sent_a_later_book() {
        // The ask shrinks at every push, so that each push makes commands.
        let lines: Vec<String> = [(1_000, "1.0"), (2_000, "0.9"), (3_000, "0.8")]
            .into_iter()
            .flat_map(|(time, ask)| both(time, ask))
            .collect();
        let (per_line, _) = flow_run(&lines);
        let made: Vec<bool> = per_line
            .iter()
            .map(|commands| !commands.is_empty())
            .collect();
        // Push 1,000 is settled by the second book at 2,000, push 2,000 by
        // the second book at 3,000, and push 3,000 only by the end.
        assert_eq!(made, [false, false, false, true, false, true, true]);
        let all: Vec<(Ts, Command)> = per_line.concat();
        assert_eq!(all, load_lines(&lines).expect("loads").commands);
    }

    #[test]
    fn a_trade_that_arrives_after_its_push_was_released_goes_to_the_next_push() {
        let mut lines: Vec<String> = [1_000, 2_000, 3_000]
            .into_iter()
            .flat_map(|time| both(time, "1.0"))
            .collect();
        // Written after the second book at 3,000, which released push 2,000,
        // although its time is before that push.
        lines.push(trade_line(1_500, "B", 81_001));
        lines.extend(both(4_000, "1.0"));
        let (per_line, stats) = flow_run(&lines);
        assert_eq!(stats.late_trades, 1);
        assert_eq!(stats.dropped_late_trades, 0);
        let all: Vec<(Ts, Command)> = per_line.concat();
        let first_at_3_000 = all
            .iter()
            .position(|&(ts, _)| ts == 3_000_000)
            .expect("a command at push 3,000");
        assert!(is_ioc(&all[first_at_3_000].1));
        assert!(!all.iter().any(|&(ts, _)| ts == 1_500_000));
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.late_trades, 1);
        assert_eq!(loaded.commands, all);
    }

    #[test]
    fn a_trade_written_before_its_push_is_not_late() {
        let mut lines: Vec<String> = both(1_000, "1.0").to_vec();
        lines.push(trade_line(1_500, "B", 81_001));
        lines.extend(both(2_000, "1.0"));
        lines.extend(both(3_000, "1.0"));
        let (_, stats) = flow_run(&lines);
        assert_eq!(stats.late_trades, 0);
    }

    #[test]
    fn a_reset_keeps_the_engines_book_and_starts_the_connections_over() {
        let mut flow = L2Flow::new(engine::market(), 2);
        let mut commands = Vec::new();
        let mut number = 0;
        let mut feed = |flow: &mut L2Flow, lines: &[String]| {
            let mut made = Vec::new();
            for line in lines {
                number += 1;
                made.extend(flow.line(number, line).expect("accepts the line"));
            }
            made
        };
        let first: Vec<String> = [1_000, 2_000]
            .into_iter()
            .flat_map(|time| both(time, "1.0"))
            .collect();
        commands.extend(feed(&mut flow, &first));
        flow.reset_connection();
        // The new connections start with the backlog of trades, which is
        // dropped, and with a book that has moved on.
        let second: Vec<String> = [trade_line(1_500, "B", 81_001)]
            .into_iter()
            .chain(both(5_000, "0.8"))
            .chain(both(6_000, "0.8"))
            .collect();
        commands.extend(feed(&mut flow, &second));
        let (rest, stats) = flow.finish();
        commands.extend(rest);
        assert_eq!(stats.dropped_trades, 1);
        assert_eq!(stats.late_trades, 0);
        assert_eq!(stats.divergence, Divergence::default());
        // One order per level: the second connection's first push replaced
        // the first connection's book instead of adding to it.
        assert_eq!(
            after(commands),
            (
                Levels::from([(81_000, 100_000), (80_999, 200_000)]),
                Levels::from([(81_001, 80_000)])
            )
        );
    }

    #[test]
    fn a_reset_forgets_the_pushes_still_open() {
        let mut flow = L2Flow::new(engine::market(), 2);
        // Only the fine grouping has sent push 2,000 when the connections
        // are lost.
        for (number, line) in [fine(1_000, "1.0"), coarse(1_000, "1.0"), fine(2_000, "1.0")]
            .iter()
            .enumerate()
        {
            flow.line(number + 1, line).expect("accepts the line");
        }
        flow.reset_connection();
        for (number, line) in both(3_000, "1.0")
            .iter()
            .chain(&both(4_000, "1.0"))
            .enumerate()
        {
            flow.line(number + 10, line).expect("accepts the line");
        }
        let (_, stats) = flow.finish();
        // The half push at 2,000 was forgotten, so it is counted as nothing
        // (kept, it would be a start-up push of the new connections).
        assert_eq!(stats.startup_pushes, 0);
        assert_eq!(stats.skipped_pushes, 0);
        assert_eq!(stats.trailing_pushes, 0);
        assert_eq!(stats.failed_merges, 0);
    }

    #[test]
    fn a_trade_at_the_time_of_a_push_comes_before_its_snapshot() {
        let lines = [
            both(1_000, "1.0").to_vec(),
            vec![trade_line(2_000, "B", 81_001)],
            both(2_000, "0.8").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.divergence, Divergence::default());
        assert_eq!(loaded.dropped_trades, 0);
        assert_eq!(loaded.dropped_late_trades, 0);
        let at_push = times(&loaded)
            .iter()
            .position(|&ts| ts == 2_000_000)
            .expect("a command at the push");
        assert!(is_ioc(&loaded.commands[at_push].1));
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 80_000)]));
    }

    #[test]
    fn a_grouping_may_send_two_books_at_one_push_time() {
        // A change at the time of the book it follows, uncompressed as the
        // source sends small ones.
        let change = r#"{"channel":"l2","data":{"u":{"c":"BTC","t":1000,"l":[[],[]],"r":[[],[]]}},"subscription":{"c":"BTC","s":4,"type":"l2"}}"#;
        let lines = [
            vec![fine(1_000, "1.0"), coarse(1_000, "1.0"), change.to_owned()],
            both(2_000, "0.5").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.skipped_pushes, 0);
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 50_000)]));
    }

    #[test]
    fn a_push_is_settled_by_the_slowest_connection() {
        // The fine connection runs two pushes ahead of the coarse one.
        let lines = [
            fine(1_000, "1.0"),
            coarse(1_000, "1.0"),
            fine(2_000, "0.8"),
            fine(3_000, "0.5"),
            coarse(2_000, "0.8"),
            coarse(3_000, "0.5"),
        ];
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (0, 0));
        assert!(times(&loaded).contains(&2_000_000));
    }

    #[test]
    fn a_trade_waits_for_the_next_complete_push() {
        let lines = [
            both(1_000, "1.0").to_vec(),
            vec![trade_line(1_500, "B", 81_001), fine(2_000, "1.0")],
            both(3_000, "0.8").to_vec(),
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.skipped_pushes, 1);
        let seen = times(&loaded);
        assert!(!seen.contains(&2_000_000));
        let trade = seen
            .iter()
            .position(|&ts| ts == 3_000_000)
            .expect("a command");
        assert!(is_ioc(&loaded.commands[trade].1));
        assert_eq!(loaded.divergence, Divergence::default());
    }

    #[test]
    fn trades_outside_the_complete_pushes_are_dropped_and_counted() {
        let pushes = [both(1_000, "1.0"), both(2_000, "1.0")].concat();
        let plain = load_lines(&pushes).expect("loads");
        let mut lines = vec![trade_line(500, "B", 81_001), trade_line(1_000, "B", 81_001)];
        lines.extend(pushes);
        lines.extend([
            trade_line(2_500, "B", 81_001),
            trade_line(3_000, "A", 81_000),
        ]);
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.dropped_trades, 2);
        assert_eq!(loaded.dropped_late_trades, 2);
        assert_eq!(loaded.commands, plain.commands);
    }

    #[test]
    fn a_grouping_that_stops_sending_leaves_later_pushes_and_trades_out() {
        let lines = [
            both(1_000, "1.0").to_vec(),
            both(2_000, "1.0").to_vec(),
            vec![fine(3_000, "0.5"), trade_line(3_500, "B", 81_001)],
            vec![fine(4_000, "0.4")],
        ]
        .concat();
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (0, 0));
        assert_eq!(loaded.trailing_pushes, 2);
        assert_eq!(loaded.dropped_late_trades, 1);
        assert!(times(&loaded).iter().all(|&ts| ts <= 2_000_000));
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 100_000)]));
    }

    #[test]
    fn lines_of_one_connection_may_run_ahead_of_another() {
        // All fine books come first in the file, as when one connection's
        // lines are written before the other's.
        let lines = [
            fine(1_000, "1.0"),
            fine(2_000, "0.5"),
            coarse(1_000, "1.0"),
            coarse(2_000, "0.5"),
        ];
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!((loaded.startup_pushes, loaded.skipped_pushes), (0, 0));
        assert_eq!(book_after(&loaded).1, Levels::from([(81_001, 50_000)]));
    }

    #[test]
    fn a_book_older_than_a_replayed_push_names_its_line() {
        let lines = [both(1_000, "1.0"), both(2_000, "1.0")].concat();
        let err = load_lines(&[lines, vec![fine(1_000, "0.5")]].concat()).unwrap_err();
        assert!(matches!(err, LoadError::Backwards { line: 5 }), "{err}");
    }

    #[test]
    fn the_documented_book_feeds_are_not_read_when_the_l2_feed_is_there() {
        let crossed = r#"{"channel":"bbo","data":{"coin":"BTC","time":1500,"bbo":[{"px":"84001.0","sz":"1.0","n":1},{"px":"84000.0","sz":"1.0","n":1}]}}"#;
        let other_coin = FIRST.replace("BTC", "ETH");
        let pushes = [both(1_000, "1.0"), both(2_000, "0.5")].concat();
        let plain = load_lines(&pushes).expect("loads");
        let mut lines = vec![crossed.to_owned(), other_coin];
        lines.extend(pushes);
        lines.extend([SECOND.to_owned()]);
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.skipped_book_lines, 3);
        assert_eq!(loaded.skipped_bbos, 0);
        assert_eq!(loaded.dropped_top_snapshots, 0);
        assert_eq!(loaded.commands, plain.commands);
    }

    #[test]
    fn error_lines_inside_an_l2_recording_are_skipped_and_counted() {
        let error = r#"{"channel":"error","data":"Already subscribed"}"#.to_owned();
        let pushes = [both(1_000, "1.0"), both(2_000, "0.5")].concat();
        let plain = load_lines(&pushes).expect("loads");
        let mut lines = vec![error.clone()];
        lines.extend(pushes);
        lines.push(error);
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.skipped_error_lines, 2);
        assert_eq!(loaded.commands, plain.commands);
    }

    #[test]
    fn an_l2_recording_without_a_complete_push_is_rejected() {
        let lines = [fine(1_000, "1.0"), coarse(1_500, "1.0")];
        assert!(matches!(load_lines(&lines), Err(LoadError::NoSnapshot)));
    }

    #[test]
    fn a_subscription_that_is_not_a_btc_grouping_names_its_line() {
        let good = fine(1_000, "1.0");
        let bare = [(80_999, "1.0")];
        let ask = [(81_001, "1.0")];
        let bad = [
            good.replace(r#","subscription":{"c":"BTC","s":5,"type":"l2"}"#, ""),
            good.replace(r#""s":5,"#, r#""m":2,"#),
            good.replace(r#""s":5,"#, r#""s":"5","#),
            l2_line(7, None, 1_000, &bare, &ask),
            l2_line(4, Some(2), 1_000, &bare, &ask),
            l2_line(5, Some(3), 1_000, &bare, &ask),
            l2_for("ETH", 5, None, 1_000, &bare, &ask),
        ];
        for line in bad {
            let err = load_lines(&[ACK.to_owned(), line.clone()]).unwrap_err();
            assert!(
                matches!(err, LoadError::Subscription { line: 2 }),
                "{line}: {err}"
            );
        }
    }

    #[test]
    fn a_subscription_without_s_is_the_grouping_without_rounding() {
        // The same book as the $1 grouping, so a recording of it with either
        // spelling of "no rounding" gives the commands the $1 one does.
        let expected = load_lines(&both(1_000, "1.0")).expect("loads");
        for none in ["", r#""s":null,"#] {
            let exact = fine(1_000, "1.0").replace(r#""s":5,"#, none);
            assert_ne!(exact, fine(1_000, "1.0"));
            let loaded = load_lines(&[exact, coarse(1_000, "1.0")]).expect("loads");
            assert_eq!(loaded.commands, expected.commands, "{none}");
            assert_eq!(loaded.failed_merges, 0);
            assert_eq!(loaded.conflicting_qty, 0);
        }
    }

    #[test]
    fn the_grouping_without_rounding_is_one_more_grouping_a_push_needs() {
        let exact = |time| fine(time, "1.0").replace(r#""s":5,"#, "");
        // At 1,500 the exact grouping sends nothing, so that push is
        // incomplete although the other two groupings sent it.
        let lines = [
            exact(1_000),
            fine(1_000, "1.0"),
            coarse(1_000, "1.0"),
            fine(1_500, "1.0"),
            coarse(1_500, "1.0"),
            exact(2_000),
            fine(2_000, "1.0"),
            coarse(2_000, "1.0"),
        ];
        let loaded = load_lines(&lines).expect("loads");
        assert_eq!(loaded.skipped_pushes, 1);
        assert_eq!(loaded.failed_merges, 0);
    }

    #[test]
    fn a_bad_subscription_and_a_bad_trade_are_reported_in_line_order() {
        let bad_subscription = fine(1_000, "1.0").replace(r#""s":5,"#, r#""s":"5","#);
        let bad_trade = trade_line(1_500, "B", 81_001).replace("BTC", "ETH");
        let err = load_lines(&[
            fine(1_000, "1.0"),
            bad_subscription.clone(),
            bad_trade.clone(),
        ])
        .unwrap_err();
        assert!(matches!(err, LoadError::Subscription { line: 2 }), "{err}");
        let err = load_lines(&[fine(1_000, "1.0"), bad_trade, bad_subscription]).unwrap_err();
        assert!(matches!(err, LoadError::Convert { line: 2, .. }), "{err}");
    }

    #[test]
    fn a_bad_l2_change_names_its_line() {
        let change =
            r#"{"channel":"l2","data":{"c":"abc"},"subscription":{"c":"BTC","s":5,"type":"l2"}}"#;
        let err = load_lines(&[fine(1_000, "1.0"), change.to_owned()]).unwrap_err();
        assert!(matches!(err, LoadError::L2 { line: 2, .. }), "{err}");
        assert!(err.to_string().starts_with("line 2:"));
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
