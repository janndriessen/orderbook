//! Fuzz runs: seeded adversarial order flow through the matcher with every
//! invariant checked after each command.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

use generator::FuzzFlow;
use harness::Run;
use protocol::Event;

const SEEDS: u64 = 50;
const COMMANDS: usize = 2_000;

/// Runs one seed and returns every event. A failing check prints the seed,
/// command index and command, so the failure can be reproduced, then fails
/// the test with its original panic.
fn run_seed(seed: u64) -> Vec<Event> {
    let mut run = Run::new();
    let mut all = Vec::new();
    for (index, command) in FuzzFlow::new(seed).take(COMMANDS).enumerate() {
        match catch_unwind(AssertUnwindSafe(|| run.apply(command))) {
            Ok(events) => all.extend(events),
            Err(payload) => {
                eprintln!("seed {seed}, command {index}: {command:?}");
                resume_unwind(payload);
            }
        }
    }
    all
}

#[test]
fn fuzz_runs_keep_every_invariant() {
    for seed in 0..SEEDS {
        run_seed(seed);
    }
}

#[test]
fn a_seed_gives_the_same_events_every_run() {
    assert_eq!(run_seed(7), run_seed(7));
}
