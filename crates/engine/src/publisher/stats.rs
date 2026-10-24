//! Throughput counters for the publisher's periodic log line.

use std::time::Duration;

use protocol::Ts;

/// Length of one reporting window.
pub const WINDOW: Duration = Duration::from_secs(10);

/// Counters for the updates sent since the last report.
///
/// Plain integers that reset per window: enough to see message rates, sizes
/// and lag without a histogram.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    updates: u64,
    events: u64,
    bytes: u64,
    max_bytes: u64,
    max_events: u64,
    max_lag_micros: u64,
}

impl Stats {
    /// Counts one update with `events` forwarded events and `bytes` on the
    /// wire. `newest_ts` is the venue time of its newest event, if it has
    /// one, and `now` the wall clock, both in microseconds since the epoch.
    ///
    /// The lag `now - newest_ts` only means something when the source runs
    /// in real time against the wall clock; for a replay at another speed it
    /// measures the gap between the recording's clock and ours. A timestamp
    /// ahead of `now` counts as no lag.
    pub fn record(&mut self, events: usize, bytes: usize, newest_ts: Option<Ts>, now: Ts) {
        let (events, bytes) = (events as u64, bytes as u64);
        self.updates += 1;
        self.events += events;
        self.bytes += bytes;
        self.max_bytes = self.max_bytes.max(bytes);
        self.max_events = self.max_events.max(events);
        if let Some(ts) = newest_ts {
            self.max_lag_micros = self.max_lag_micros.max(now.saturating_sub(ts));
        }
    }

    /// The largest lag of the window in whole milliseconds, the unit the log
    /// reports it in.
    pub fn max_lag_ms(&self) -> u64 {
        self.max_lag_micros / 1_000
    }

    /// Logs the window as one line. Silent windows without an update are
    /// skipped, since the heartbeat already shows the feed is alive.
    pub fn log(&self) {
        if self.updates == 0 {
            return;
        }
        tracing::info!(
            window_secs = WINDOW.as_secs(),
            updates = self.updates,
            events = self.events,
            bytes = self.bytes,
            max_update_bytes = self.max_bytes,
            max_update_events = self.max_events,
            max_lag_ms = self.max_lag_ms(),
            "publisher throughput"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_sums_counts_and_keeps_the_largest_values() {
        let mut stats = Stats::default();
        stats.record(3, 100, Some(1_000), 5_000);
        stats.record(10, 400, Some(9_000), 9_500);
        stats.record(0, 50, None, 20_000);

        assert_eq!(
            stats,
            Stats {
                updates: 3,
                events: 13,
                bytes: 550,
                max_bytes: 400,
                max_events: 10,
                max_lag_micros: 4_000,
            }
        );
    }

    #[test]
    fn the_lag_is_reported_in_whole_milliseconds() {
        let mut stats = Stats::default();
        assert_eq!(stats.max_lag_ms(), 0);
        stats.record(1, 10, Some(1_000), 5_999);
        assert_eq!(stats.max_lag_ms(), 4);
        stats.record(1, 10, Some(0), 12_000_000);
        assert_eq!(stats.max_lag_ms(), 12_000);
    }

    #[test]
    fn a_timestamp_ahead_of_the_clock_counts_as_no_lag() {
        let mut stats = Stats::default();
        stats.record(1, 10, Some(9_000), 1_000);
        assert_eq!(stats.max_lag_micros, 0);
    }
}
