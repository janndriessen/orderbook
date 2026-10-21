//! Fuzz runs: seeded adversarial order flow through the matcher with every
//! invariant checked after each command and every event list compared with the
//! reference model's.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use generator::FuzzFlow;
use harness::{ReferenceMatcher, Run};
use protocol::{Command, Event};

const SEEDS: u64 = 50;
const COMMANDS: usize = 2_000;
const MIN_KEPT_WITH_QUEUE: usize = 150;
const MIN_PRICE_CHANGES: usize = 900;

/// Runs one seed and returns its events, how many replaces kept their
/// priority with another order queued behind them (same price, quantity not
/// growing), and how many replaces changed a resting order's price. A failing
/// check or event mismatch prints the seed, command index and command, so the
/// failure can be reproduced, then fails the test with its original panic.
///
/// The generator rarely replaces a resting order at its current price, so
/// every other replace of a resting order is rewritten to that price, and
/// every fourth one also to its quantity, which makes it change nothing. The
/// rewrites follow a counter, which keeps the run deterministic.
fn run_seed(seed: u64) -> (Vec<Event>, usize, usize) {
    let mut run = Run::new();
    let mut reference = ReferenceMatcher::new();
    let mut events = Vec::new();
    let (mut kept_with_queue, mut price_changes) = (0, 0);
    let mut replaces = 0;
    for (index, mut command) in FuzzFlow::new(seed).take(COMMANDS).enumerate() {
        let (mut keeps, mut moves) = (false, false);
        if let Command::Replace {
            order_id,
            price,
            qty,
        } = &mut command
            && let Some(resting) = run.book().order(*order_id)
        {
            replaces += 1;
            if replaces % 2 == 0 {
                *price = resting.price;
            }
            if replaces % 4 == 0 {
                *qty = resting.qty;
            }
            moves = *price != resting.price;
            let queue: Vec<_> = run
                .book()
                .level(resting.side, resting.price)
                .map(|level| level.orders().collect())
                .unwrap_or_default();
            let position = queue.iter().position(|id| *id == resting.order_id);
            keeps =
                !moves && *qty <= resting.qty && position.is_some_and(|at| at + 1 < queue.len());
        }
        let checked = catch_unwind(AssertUnwindSafe(|| {
            let produced = run.apply(command);
            assert_eq!(
                produced,
                reference.apply(command),
                "differs from the reference model"
            );
            produced
        }));
        match checked {
            Ok(produced) => {
                events.extend(produced);
                kept_with_queue += usize::from(keeps);
                price_changes += usize::from(moves);
            }
            Err(payload) => {
                eprintln!("seed {seed}, command {index}: {command:?}");
                resume_unwind(payload);
            }
        }
    }
    (events, kept_with_queue, price_changes)
}

#[test]
fn fuzz_runs_match_the_reference_and_keep_every_invariant() {
    let (mut kept, mut moved) = (0, 0);
    for seed in 0..SEEDS {
        let (_, seed_kept, seed_moved) = run_seed(seed);
        kept += seed_kept;
        moved += seed_moved;
    }
    // Empirical bounds with headroom: these seeds give about 290 and 1,800.
    assert!(
        kept >= MIN_KEPT_WITH_QUEUE,
        "only {kept} replaces kept their priority with an order queued behind"
    );
    assert!(
        moved >= MIN_PRICE_CHANGES,
        "only {moved} replaces changed a price"
    );
}

#[test]
fn a_seed_gives_the_same_events_every_run() {
    assert_eq!(run_seed(7).0, run_seed(7).0);
}
