//! The `levels` binary: measures how long price levels get in a recording and
//! what a cancel costs on the replayed book, to decide whether a `VecDeque`
//! per level is enough.
//!
//! `REPLAY_PATH` names a recording made by `record`. Run it from a release
//! build; a debug build says nothing about cancel cost. Each cancel is timed
//! around `Matcher::apply` with `Instant`, whose reading costs a few tens of
//! nanoseconds itself, so the figures are slight overestimates.

use std::hint::black_box;
use std::time::Instant;

use anyhow::Context;
use hyperliquid_adapter::Message;
use matching::Matcher;
use protocol::{Command, Side};
use replayer::load;

/// The nearest-rank percentile `p` (0 to 100) of `sorted`, or 0 when empty.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    let rank = (sorted.len() as f64 * p / 100.0).ceil() as usize;
    sorted.get(rank.saturating_sub(1)).copied().unwrap_or(0)
}

/// Sorts `samples` and prints one line of statistics.
fn report(name: &str, samples: &mut [u64]) {
    samples.sort_unstable();
    println!(
        "  {name:<14} count {:>10}  median {:>6}  p90 {:>6}  p99 {:>6}  max {:>6}",
        samples.len(),
        percentile(samples, 50.0),
        percentile(samples, 90.0),
        percentile(samples, 99.0),
        samples.last().copied().unwrap_or(0),
    );
}

/// The order counts `n` the source reports per level, over all levels and
/// over the best bid and best ask only.
///
/// Snapshots with at most 5 levels on each side are the top-of-book feed, not
/// full snapshots, and are skipped so that they do not dilute the statistics.
fn source_counts(recording: &str) -> (Vec<u64>, Vec<u64>) {
    let (mut all, mut touch) = (Vec::new(), Vec::new());
    for text in recording.lines() {
        // Acknowledgements, pongs and a cut-off last line do not parse.
        let Ok(Message::L2Book(book)) = serde_json::from_str(text) else {
            continue;
        };
        if book.levels.iter().all(|side| side.len() <= 5) {
            continue;
        }
        for side in &book.levels {
            all.extend(side.iter().map(|level| level.n));
            if let Some(best) = side.first() {
                touch.push(best.n);
            }
        }
    }
    (all, touch)
}

fn main() -> anyhow::Result<()> {
    let path = std::env::var("REPLAY_PATH").context("REPLAY_PATH must name a recording")?;
    let recording =
        std::fs::read_to_string(&path).with_context(|| format!("cannot read {path}"))?;
    let loaded =
        load(&recording, &engine::market()).with_context(|| format!("cannot load {path}"))?;

    println!("recording: {path}");
    println!(
        "commands: {}, incomplete last line left out: {}",
        loaded.commands.len(),
        loaded.incomplete_last_line
    );

    let (mut source_all, mut source_touch) = source_counts(&recording);
    println!("\norders per level as the source reports it (n), over all snapshots");
    report("all levels", &mut source_all);
    report("best bid/ask", &mut source_touch);

    // Level lengths are sampled after every command; only the apply is timed.
    let mut matcher = Matcher::new();
    let (mut ours_all, mut ours_touch) = (Vec::new(), Vec::new());
    let mut cancel_ns: Vec<u64> = Vec::new();
    for &(_, command) in &loaded.commands {
        let start = Instant::now();
        black_box(matcher.apply(black_box(command)));
        if matches!(command, Command::Cancel { .. }) {
            let ns = start.elapsed().as_nanos() as u64;
            cancel_ns.push(ns);
        }
        for side in [Side::Buy, Side::Sell] {
            let counts: Vec<u64> = matcher
                .book()
                .levels(side)
                .map(|(_, level)| level.orders().count() as u64)
                .collect();
            ours_all.extend(&counts);
            // Levels ascend by price: the best bid is last, the best ask first.
            let touch = if side == Side::Buy {
                counts.last()
            } else {
                counts.first()
            };
            if let Some(&n) = touch {
                ours_touch.push(n);
            }
        }
    }

    println!("\norders per level in our replayed book, sampled after every command");
    report("all levels", &mut ours_all);
    report("best bid/ask", &mut ours_touch);

    println!("\ncancel commands in the replay, timed around Matcher::apply (ns)");
    if cancel_ns.is_empty() {
        println!("  none");
    } else {
        let mean = cancel_ns.iter().sum::<u64>() as f64 / cancel_ns.len() as f64;
        cancel_ns.sort_unstable();
        println!(
            "  count {}  mean {mean:.1}  p50 {}  p99 {}  max {}",
            cancel_ns.len(),
            percentile(&cancel_ns, 50.0),
            percentile(&cancel_ns, 99.0),
            percentile(&cancel_ns, 100.0),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn empty_and_single_samples() {
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[7], 0.0), 7);
        assert_eq!(percentile(&[7], 50.0), 7);
        assert_eq!(percentile(&[7], 100.0), 7);
    }

    #[test]
    fn nearest_rank_positions() {
        let ten: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&ten, 10.0), 1);
        assert_eq!(percentile(&ten, 11.0), 2);
        assert_eq!(percentile(&ten, 50.0), 5);
        assert_eq!(percentile(&ten, 90.0), 9);
        assert_eq!(percentile(&ten, 99.0), 10);
        assert_eq!(percentile(&ten, 100.0), 10);
    }
}
