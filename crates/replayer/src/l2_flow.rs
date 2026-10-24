//! Assembling the `l2` feed into commands, one line at a time.
//!
//! The feed has one subscription per price grouping, each on its own
//! connection, so the books of one push arrive in no fixed order and a little
//! apart. [`L2Flow`] collects them by push time, merges every push that all
//! groupings have sent a book for, translates the merged book, and holds the
//! trades to the next merged push. It works the same on a finished recording
//! and on a live connection, because it only ever looks at lines already seen.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use hyperliquid_adapter::{
    BookSnapshot, Divergence, Grouping, L2Decoder, MergeError, Merged, Message, Trade, Translator,
    WireL2, merge,
};
use protocol::{Command, MarketInfo, Ts};
use tracing::{debug, warn};

use crate::{Classified, Feed, LoadError, classify, message, mismatch};

/// The books the groupings sent at one push time.
type Push = BTreeMap<Grouping, BookSnapshot>;

/// Commands with their source times.
type Commands = Vec<(Ts, Command)>;

/// What an [`L2Flow`] left out or noticed on the way, as counts.
#[derive(Debug, Default)]
pub struct L2Stats {
    /// Whether a push has merged yet, which starts the book.
    pub started: bool,
    /// How many pushes have merged and been released, across resets.
    pub merged_pushes: usize,
    /// How far the translator's book is known to differ from the source's.
    pub divergence: Divergence,
    /// Trades at or before the first push that merged, which were left out:
    /// the backlog the source sends on subscribing.
    pub dropped_trades: usize,
    /// Trades after the last push that merged, which were left out because no
    /// push came to release them. Known only once the flow is finished.
    pub dropped_late_trades: usize,
    /// Trades that arrived after the push they belong to (the first merged
    /// push at or after their time) was already released, and were held to
    /// the next one instead.
    pub late_trades: usize,
    /// Pushes merged and translated again because a grouping sent a second
    /// book at the time of a push already released.
    pub reapplied_pushes: usize,
    /// Books skipped after a reset because their time was at or before a push
    /// already released: the source's clock stepped back.
    pub stale_books: usize,
    /// Push times before the one at which the last grouping sent its first
    /// book, which were left out.
    pub startup_pushes: usize,
    /// Push times from there on at which some grouping sent no book although
    /// it sent a later one, which were left out.
    pub skipped_pushes: usize,
    /// Push times still open when the flow finished, at which some grouping
    /// sent no book and none later, which were left out.
    pub trailing_pushes: usize,
    /// Complete pushes whose groupings could not be merged, which were left
    /// out.
    pub failed_merges: usize,
    /// The time of the first push that failed to merge, and why.
    pub first_merge_error: Option<(Ts, MergeError)>,
    /// How far the merged books missed their inputs, in lots, summed.
    pub conflicting_qty: u64,
    /// Lines of the documented book feeds (`l2Book` and `bbo`) fed through
    /// [`L2Flow::line`], which were not read.
    pub skipped_book_lines: usize,
    /// `error` lines fed through [`L2Flow::line`], which were left out.
    pub skipped_error_lines: usize,
}

/// Turns the lines of the `l2` feed, and the trades that come with it, into
/// commands for the engine.
///
/// A push is complete when every one of the flow's groupings has a book at
/// exactly its time, and it is merged and translated, and its commands
/// emitted, the moment it is. An incomplete push is settled once every
/// grouping has sent a book after it (any later book of a grouping says that
/// it sends none for the push), and is left out and counted. Pushes are
/// handled in time order. A grouping can send a second book at the time of
/// the push released last (the decoder accepts a change with the same time);
/// the push is then merged and translated again with it. Trades are translated at
/// the time of the first merged push at or after their own time, before that
/// push's snapshot, so the book changes only at pushes, as in the source's
/// web app. A trade whose push was already settled when it arrived goes to
/// the next merged push and is counted in [`L2Stats::late_trades`].
#[derive(Debug)]
pub struct L2Flow {
    market: MarketInfo,
    /// Groupings a complete push has books of.
    groupings: usize,
    /// Kept across [`reset_connection`](Self::reset_connection), so that the
    /// engine's book, mirrored from it, is carried over to the next
    /// connection's first push.
    translator: Translator,
    /// The latest push time settled, merged or not. Kept across a reset, so
    /// that time never goes backwards.
    done_through: Option<Ts>,
    /// The latest push time that merged.
    last_merged: Option<Ts>,
    decoders: BTreeMap<Grouping, L2Decoder>,
    /// Each grouping's latest book time.
    latest: BTreeMap<Grouping, Ts>,
    /// The time at which the last grouping sent its first book.
    all_started: Ts,
    pending: BTreeMap<Ts, Push>,
    /// The complete push released last, with the books it was made from, for
    /// a grouping that sends another book at the same time.
    released: Option<(Ts, Push)>,
    /// Set by a reset until every grouping has a book past `done_through`:
    /// books at or before it are then skipped instead of an error.
    recovering: bool,
    warned_stale: bool,
    /// Each grouping's latest skipped book time since the last reset, to tell
    /// a source catching up from one whose clock stands still.
    stale_latest: BTreeMap<Grouping, Ts>,
    /// Skipped books whose time was later than any skipped before from the
    /// same grouping.
    catching_up: usize,
    /// Whether the push in `released` merged when it was released.
    released_merged: bool,
    /// The time of the first push that merged since the connections were
    /// opened; trades at or before it are the backlog.
    first_merged: Option<Ts>,
    /// Trades in time order that no merged push has released yet.
    trades: Vec<Trade>,
    out: Commands,
    stats: L2Stats,
}

impl L2Flow {
    /// A flow for books of `groupings` price groupings per push, with prices
    /// and sizes converted for `market`.
    pub fn new(market: MarketInfo, groupings: usize) -> Self {
        Self {
            market,
            groupings,
            translator: Translator::new(),
            done_through: None,
            last_merged: None,
            decoders: BTreeMap::new(),
            latest: BTreeMap::new(),
            all_started: 0,
            pending: BTreeMap::new(),
            released: None,
            released_merged: false,
            recovering: false,
            warned_stale: false,
            stale_latest: BTreeMap::new(),
            catching_up: 0,
            first_merged: None,
            trades: Vec::new(),
            out: Vec::new(),
            stats: L2Stats::default(),
        }
    }

    /// Feeds one line of a recording or of the live connections, numbered
    /// `number` in error messages, and returns the commands that became
    /// ready: an `l2` line settles the pushes every grouping is past, a
    /// `trades` line holds its trades. Housekeeping lines, the documented
    /// book feeds and `error` lines make no commands.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadError`] for a line that is malformed, on another
    /// channel, not convertible, or an `l2` line without a valid
    /// subscription or that does not apply to its grouping's book.
    pub fn line(&mut self, number: usize, text: &str) -> Result<Commands, LoadError> {
        match classify(number, text, false) {
            Classified::Skip | Classified::Cut => Ok(Vec::new()),
            Classified::ErrorLine => {
                self.stats.skipped_error_lines += 1;
                Ok(Vec::new())
            }
            Classified::Feed(feed) => match feed {
                Feed::Failed(error) => Err(error),
                Feed::L2Book | Feed::Bbo => {
                    self.stats.skipped_book_lines += 1;
                    Ok(Vec::new())
                }
                Feed::Trades => {
                    let Message::Trades(wires) = message(number, text)? else {
                        return Err(mismatch(number));
                    };
                    let mut trades = Vec::with_capacity(wires.len());
                    for wire in &wires {
                        let convert = |source| LoadError::Convert {
                            line: number,
                            source,
                        };
                        trades.push(wire.to_trade(&self.market).map_err(convert)?);
                    }
                    self.trades(trades);
                    Ok(Vec::new())
                }
                Feed::L2(grouping) => {
                    let Message::L2(wire) = message(number, text)? else {
                        return Err(mismatch(number));
                    };
                    self.book(number, grouping, &wire)
                }
            },
        }
    }

    /// Holds trades for the next merged push. A trade at or before the first
    /// push that merged is the backlog the source sends on subscribing, and
    /// is dropped and counted.
    pub fn trades(&mut self, trades: Vec<Trade>) {
        for trade in trades {
            if self.first_merged.is_some_and(|first| trade.ts <= first) {
                self.stats.dropped_trades += 1;
                continue;
            }
            if self.last_merged.is_some_and(|last| trade.ts <= last) {
                self.stats.late_trades += 1;
            }
            // After every trade that is not later, so that trades of one time
            // stay in the order they arrived in.
            let at = self.trades.partition_point(|held| held.ts <= trade.ts);
            self.trades.insert(at, trade);
        }
    }

    /// Applies an `l2` message of `grouping`, read from line `line`, and
    /// returns the commands of the pushes it settles.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::L2`] when the message does not apply to the
    /// grouping's book, and [`LoadError::Backwards`] when it is at or before
    /// a push already settled.
    pub fn book(
        &mut self,
        line: usize,
        grouping: Grouping,
        wire: &WireL2,
    ) -> Result<Commands, LoadError> {
        let book = self
            .decoders
            .entry(grouping)
            .or_default()
            .apply(wire, &self.market)
            .map_err(|source| LoadError::L2 { line, source })?
            .clone();
        let ts = book.ts;
        // Another book at the time of the push released last: the push is
        // merged and translated again with it.
        if self.done_through == Some(ts)
            && let Some((released, push)) = self.released.as_mut()
            && *released == ts
        {
            push.insert(grouping, book);
            let push = push.clone();
            self.reapply(ts, &push);
            return Ok(std::mem::take(&mut self.out));
        }
        if self.done_through.is_some_and(|done| ts <= done) {
            if !self.recovering {
                return Err(LoadError::Backwards { line });
            }
            // New connections of a source whose clock stepped back: its books
            // are skipped until its time passes what was already replayed.
            self.stats.stale_books += 1;
            if self
                .stale_latest
                .get(&grouping)
                .is_none_or(|&seen| ts > seen)
            {
                self.stale_latest.insert(grouping, ts);
                self.catching_up += 1;
            }
            if !std::mem::replace(&mut self.warned_stale, true) {
                warn!(ts, done_through = ?self.done_through, "the source's time is behind what was already replayed, skipping its books until it catches up");
            }
            return Ok(Vec::new());
        }
        self.pending.entry(ts).or_default().insert(grouping, book);
        match self.latest.entry(grouping) {
            Entry::Vacant(entry) => {
                self.all_started = self.all_started.max(ts);
                entry.insert(ts);
            }
            Entry::Occupied(mut entry) => {
                entry.insert(ts.max(*entry.get()));
            }
        }
        // Every grouping has sent a book after the pushes before the earliest
        // of the latest ones, so none of them can still complete.
        if self.latest.len() == self.groupings {
            self.recovering = false;
            let settled = self.latest.values().min().copied().unwrap_or(0);
            while let Some(entry) = self.pending.first_entry()
                && *entry.key() < settled
            {
                let (ts, push) = entry.remove_entry();
                self.settle(ts, &push, false);
            }
            // A push every grouping has a book of does not wait for later
            // books: nothing can be added to it but a second book of one
            // grouping at the same time, which is merged in afterwards.
            if self
                .pending
                .get(&ts)
                .is_some_and(|push| push.len() == self.groupings)
                && let Some(push) = self.pending.remove(&ts)
            {
                self.settle(ts, &push, false);
            }
        }
        Ok(std::mem::take(&mut self.out))
    }

    /// Forgets everything about the connections that was learned from them
    /// (the books of each grouping, the pushes still open, the trades held,
    /// who has started) and keeps the translator, for the connections to be
    /// opened again. The first push that merges afterwards is mirrored in
    /// full, so the engine's book catches up with what was missed.
    pub fn reset_connection(&mut self) {
        self.decoders.clear();
        self.latest.clear();
        self.all_started = 0;
        self.pending.clear();
        self.released = None;
        self.recovering = true;
        self.warned_stale = false;
        self.stale_latest.clear();
        self.last_merged = None;
        self.first_merged = None;
        self.trades.clear();
        self.out.clear();
    }

    /// How many pushes have merged and been released so far plus how many
    /// books were skipped for being behind what was already replayed while
    /// their grouping's time moved forward. A caller that sees it stand still
    /// while lines keep coming knows the groupings' times no longer match, or
    /// that a grouping's clock is frozen. A skipped book with a later time
    /// than the grouping's earlier ones counts because the source is only
    /// catching up with a clock that stepped back.
    pub fn progress(&self) -> usize {
        self.stats.merged_pushes + self.catching_up
    }

    /// The time of the latest push that merged since the connections were
    /// opened, if one has. A push that leaves the book unchanged merges
    /// without commands, so this is how a caller sees that the new
    /// connections deliver.
    pub fn last_merged(&self) -> Option<Ts> {
        self.last_merged
    }

    /// Whether a push has merged yet.
    pub fn started(&self) -> bool {
        self.stats.started
    }

    /// Ends the input: settles the pushes still open, counts the trades no
    /// push came to release, and returns the commands of those pushes with
    /// the counts.
    pub fn finish(mut self) -> (Commands, L2Stats) {
        for (ts, push) in std::mem::take(&mut self.pending) {
            self.settle(ts, &push, true);
        }
        self.stats.dropped_late_trades = self.trades.len();
        self.stats.divergence = self.translator.divergence();
        (self.out, self.stats)
    }

    /// Merges and translates the push at `ts` again after a grouping sent
    /// another book at that time. The translator mirrors whole snapshots, so
    /// this only brings the engine's book to the later books.
    fn reapply(&mut self, ts: Ts, push: &Push) {
        let inputs: Vec<(Grouping, &BookSnapshot)> = push
            .iter()
            .map(|(&grouping, book)| (grouping, book))
            .collect();
        match merge(&inputs) {
            Ok(merged) => {
                self.stats.reapplied_pushes += 1;
                if self.released_merged {
                    self.out.extend(self.translator.on_snapshot(&merged.book));
                } else {
                    // The push did not merge when it was released; this is
                    // the release it never had.
                    self.released_merged = true;
                    self.release(ts, &merged);
                }
            }
            Err(error) => {
                debug!(ts, %error, "push at the same time does not merge, left as it was")
            }
        }
    }

    /// Replays or leaves out the push at `ts`. `open` is whether the input
    /// ended before the push was settled.
    fn settle(&mut self, ts: Ts, push: &Push, open: bool) {
        self.done_through = Some(ts);
        if push.len() < self.groupings {
            if ts < self.all_started {
                self.stats.startup_pushes += 1;
            } else if open {
                self.stats.trailing_pushes += 1;
            } else {
                self.stats.skipped_pushes += 1;
            }
            return;
        }
        self.released = Some((ts, push.clone()));
        self.released_merged = false;
        let inputs: Vec<(Grouping, &BookSnapshot)> = push
            .iter()
            .map(|(&grouping, book)| (grouping, book))
            .collect();
        let merged = match merge(&inputs) {
            Ok(merged) => merged,
            Err(error) => {
                self.stats.failed_merges += 1;
                self.stats.first_merge_error.get_or_insert((ts, error));
                return;
            }
        };
        self.released_merged = true;
        self.release(ts, &merged);
    }

    /// Emits the commands of the merged push at `ts`: the trades it holds,
    /// then its snapshot. The first one since the connections were opened
    /// starts the book, and drops the trades up to it as the backlog.
    fn release(&mut self, ts: Ts, merged: &Merged) {
        self.stats.conflicting_qty += merged.conflicting_qty;
        let due = self.trades.partition_point(|trade| trade.ts <= ts);
        let released: Vec<Trade> = self.trades.drain(..due).collect();
        if self.first_merged.is_some() {
            for trade in released {
                let held = Trade { ts, ..trade };
                self.out.extend(self.translator.on_trade(&held));
            }
        } else {
            self.first_merged = Some(ts);
            self.stats.dropped_trades += released.len();
        }
        self.stats.started = true;
        self.stats.merged_pushes += 1;
        self.last_merged = Some(ts);
        self.out.extend(self.translator.on_snapshot(&merged.book));
    }
}
