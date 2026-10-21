//! Benchmark of the matcher: throughput on fuzz order flow, and the cost of a
//! cancel as a function of the length of the price level it sits in.
//!
//! Run with `cargo run -p harness --release --bin bench`. The numbers depend
//! on the machine and must be read from a release build; a debug build is
//! far slower and says nothing about the matcher.

use std::hint::black_box;
use std::time::Instant;

use matching::Matcher;
use protocol::{Command, Price, Side, TimeInForce};

const COMMANDS: usize = 1_000_000;
const WARM_UP: usize = 100_000;
const SEED: u64 = 1;
const PRICE: Price = 100_000;
const LEVEL_LENGTHS: [u64; 5] = [10, 100, 1_000, 10_000, 100_000];
const MAX_CANCELS: u64 = 1_000;

/// Applies every command to a matcher and returns the number of events.
fn apply_all(matcher: &mut Matcher, commands: &[Command]) -> usize {
    commands
        .iter()
        .map(|&command| black_box(matcher.apply(black_box(command))).len())
        .sum()
}

fn throughput() {
    let commands: Vec<Command> = generator::FuzzFlow::new(SEED).take(COMMANDS).collect();
    apply_all(&mut Matcher::new(), &commands[..WARM_UP]);

    let mut matcher = Matcher::new();
    let start = Instant::now();
    let events = apply_all(&mut matcher, &commands);
    let secs = start.elapsed().as_secs_f64();

    println!("throughput: {COMMANDS} fuzz commands (seed {SEED}), {events} events");
    println!(
        "{:>16} {:>16} {:>16}",
        "commands/s", "events/s", "ns/command"
    );
    println!(
        "{:>16.0} {:>16.0} {:>16.1}",
        COMMANDS as f64 / secs,
        events as f64 / secs,
        secs * 1e9 / COMMANDS as f64
    );
}

/// A matcher with `length` GTC bids resting at one price, ids `1..=length`,
/// id 1 first in the queue.
fn level_of(length: u64) -> Matcher {
    let mut matcher = Matcher::new();
    for id in 1..=length {
        matcher.place_limit(id, Side::Buy, PRICE, TimeInForce::Gtc, 1);
    }
    matcher
}

/// Average nanoseconds per cancel of `ids`, in that order, on a fresh level.
///
/// The level is rebuilt for every position instead of re-placing canceled
/// orders, which would move them to the back of the queue. The caller picks
/// ids so that each one sits at the same queue index when it is canceled,
/// and the batch is at most a tenth of the level, so the length stays close
/// to its starting value.
fn cancel_ns(length: u64, ids: impl Iterator<Item = u64>) -> f64 {
    let mut matcher = level_of(length);
    let ids: Vec<u64> = ids.collect();
    let start = Instant::now();
    for &id in &ids {
        black_box(matcher.cancel(black_box(id)));
    }
    start.elapsed().as_secs_f64() * 1e9 / ids.len() as f64
}

fn cancel_cost() {
    println!("\ncancel cost by queue position in a level (ns/cancel)");
    println!(
        "{:>10} {:>10} {:>10} {:>10}",
        "length", "front", "middle", "back"
    );
    for length in LEVEL_LENGTHS {
        let n = (length / 10).clamp(1, MAX_CANCELS);
        // Canceling ids 1, 2, ... always hits the front. Canceling ids from
        // the middle upward hits the same index, since each removal shifts
        // the next order into it. Canceling from the last id downward always
        // hits the back.
        let front = cancel_ns(length, 1..=n);
        let middle = cancel_ns(length, length / 2..length / 2 + n);
        let back = cancel_ns(length, (length - n + 1..=length).rev());
        println!("{length:>10} {front:>10.1} {middle:>10.1} {back:>10.1}");
    }
}

fn main() {
    throughput();
    cancel_cost();
}
