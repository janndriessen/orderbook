//! Paces recorded order flow into steps for the service.

use std::iter;
use std::num::NonZeroU32;
use std::time::Duration;

use engine::Step;
use protocol::{Command, Ts};

/// How fast recorded order flow is played.
#[derive(Debug, Clone, Copy)]
pub enum Speed {
    /// Waits equal the recorded gaps between commands.
    RealTime,
    /// Recorded gaps divided by the factor, at nanosecond precision.
    Times(NonZeroU32),
    /// No waits: commands are submitted as fast as the service takes them.
    Max,
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
}
