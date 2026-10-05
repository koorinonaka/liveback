//! Retry/reporting gate for the playback worker's segment fetches.
//!
//! Background (incident 2026-08-14): a loaded session's buffer root was
//! renamed out from under the running app, so every
//! `read_review_segment` came back `"segment path is invalid"`. Nothing in
//! the fetch path was throttled, and the UI's range-repeat poll kept
//! re-arming the engine, so each failed attempt acquired a review lease,
//! released it and warned -- three log records per attempt, ~9,700 attempts
//! a second, 521MB of `liveback.log` in under three minutes.
//!
//! Two properties fix that, and both belong here rather than at the call
//! sites: an attempt that just failed is not retried until a growing
//! backoff has elapsed, and a failure that is just the previous one
//! repeating is not logged again. The gate is keyed per segment index
//! because the storm alternated between two of them (the repeat target and
//! the live-edge segment) -- a single "last failure" slot would have been
//! reset by every alternation and gated nothing.
//!
//! `Instant` is a parameter rather than read inside, so the schedule is
//! testable without sleeping.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Backoff after the first failure of a segment.
const FIRST_DELAY: Duration = Duration::from_millis(250);
/// Ceiling the backoff doubles up to.
const MAX_DELAY: Duration = Duration::from_secs(5);
/// A segment stuck failing still gets a record this often, carrying the
/// count of what was suppressed, so a persistent fault stays visible in the
/// log without being able to fill a disk.
const REPEAT_WARN: Duration = Duration::from_secs(30);

struct Entry {
    /// No attempt is made for this index before this instant.
    retry_at: Instant,
    /// Doubles per consecutive failure, capped at `MAX_DELAY`.
    delay: Duration,
    /// Failures swallowed since the last record went out for this index.
    suppressed: u64,
    last_warned: Instant,
}

/// What the caller should do about a failure it just recorded.
pub(super) struct Report {
    /// Whether this failure is worth a log record.
    pub(super) log: bool,
    /// Failures swallowed since the last record for this index; only
    /// meaningful (and only non-zero) when `log` is set.
    pub(super) suppressed: u64,
    /// How long the next attempt for this index is held off.
    pub(super) retry_in: Duration,
}

#[derive(Default)]
pub(super) struct FetchGate {
    /// Bounded by the playlist length: an entry only exists for an index
    /// that has failed, and it is dropped as soon as that index succeeds.
    entries: HashMap<u64, Entry>,
}

impl FetchGate {
    /// Whether `index` may be fetched now. A gated call must do nothing at
    /// all -- not even acquire a review lease, whose acquire/release pair
    /// was two thirds of the records the 2026-08-14 storm wrote.
    pub(super) fn ready(&self, index: u64, now: Instant) -> bool {
        self.entries
            .get(&index)
            .is_none_or(|entry| now >= entry.retry_at)
    }

    /// Records a failed fetch of `index` and reports whether to log it.
    pub(super) fn fail(&mut self, index: u64, now: Instant) -> Report {
        match self.entries.get_mut(&index) {
            None => {
                self.entries.insert(
                    index,
                    Entry {
                        retry_at: now + FIRST_DELAY,
                        delay: FIRST_DELAY,
                        suppressed: 0,
                        last_warned: now,
                    },
                );
                // The first failure of a segment is always worth a record:
                // it is the one that says what broke and when.
                Report {
                    log: true,
                    suppressed: 0,
                    retry_in: FIRST_DELAY,
                }
            }
            Some(entry) => {
                entry.delay = (entry.delay * 2).min(MAX_DELAY);
                entry.retry_at = now + entry.delay;
                let due = now.duration_since(entry.last_warned) >= REPEAT_WARN;
                if due {
                    let suppressed = entry.suppressed;
                    entry.suppressed = 0;
                    entry.last_warned = now;
                    Report {
                        log: true,
                        suppressed,
                        retry_in: entry.delay,
                    }
                } else {
                    entry.suppressed += 1;
                    Report {
                        log: false,
                        suppressed: 0,
                        retry_in: entry.delay,
                    }
                }
            }
        }
    }

    /// Clears `index`'s backoff. Returns how many of its failures went
    /// unlogged since the last record for it, so the caller can close the
    /// story out; `None` when the index was not failing, which is the
    /// normal path and costs a single map lookup.
    pub(super) fn succeed(&mut self, index: u64) -> Option<u64> {
        self.entries.remove(&index).map(|entry| entry.suppressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_failure_is_reported_and_the_repeat_is_not() {
        let mut gate = FetchGate::default();
        let start = Instant::now();
        assert!(gate.ready(0, start), "an index that never failed is open");

        let first = gate.fail(0, start);
        assert!(first.log, "the failure that says what broke must be logged");
        assert_eq!(first.retry_in, FIRST_DELAY);
        assert!(!gate.ready(0, start), "the retry is held off");
        assert!(
            !gate.ready(0, start + FIRST_DELAY - Duration::from_millis(1)),
            "still held off just short of the delay"
        );
        assert!(
            gate.ready(0, start + FIRST_DELAY),
            "and open again after it"
        );

        let second = gate.fail(0, start + FIRST_DELAY);
        assert!(!second.log, "the same failure repeating is not news");
        assert_eq!(second.retry_in, FIRST_DELAY * 2, "the backoff doubles");
    }

    /// The shape of the 2026-08-14 storm: two indexes failing in
    /// alternation, driven as fast as the caller could ask.
    #[test]
    fn a_storm_across_two_indexes_costs_two_records_per_warn_window() {
        let mut gate = FetchGate::default();
        let start = Instant::now();
        let mut logged = 0;
        let mut attempted = 0;
        // 60s of the UI re-arming the engine every 100us.
        for step in 0..600_000u32 {
            let now = start + Duration::from_micros(u64::from(step) * 100);
            for index in [0, 39] {
                if !gate.ready(index, now) {
                    continue;
                }
                attempted += 1;
                if gate.fail(index, now).log {
                    logged += 1;
                }
            }
        }
        // One first per index, then at most one repeat per index per
        // `REPEAT_WARN`. Single digits over a minute, against the ~580,000
        // records the same minute of the real storm wrote.
        assert!(
            (2..=8).contains(&logged),
            "logged {logged} records in 60s of storm"
        );
        assert!(
            attempted < 100,
            "the backoff must also cut the attempts (and their leases), got {attempted}"
        );
    }

    #[test]
    fn the_backoff_stops_doubling_at_the_cap() {
        let mut gate = FetchGate::default();
        let mut now = Instant::now();
        let mut report = gate.fail(7, now);
        for _ in 0..20 {
            now += report.retry_in;
            report = gate.fail(7, now);
        }
        assert_eq!(report.retry_in, MAX_DELAY);
    }

    #[test]
    fn success_clears_the_backoff_and_returns_the_suppressed_count() {
        let mut gate = FetchGate::default();
        let start = Instant::now();
        assert_eq!(gate.succeed(3), None, "an index that never failed is quiet");
        gate.fail(3, start);
        gate.fail(3, start + FIRST_DELAY);
        gate.fail(3, start + FIRST_DELAY * 3);
        assert_eq!(gate.succeed(3), Some(2), "both repeats were suppressed");
        assert!(gate.ready(3, start), "and the index is open again");

        // One index recovering must not un-gate another that is still broken:
        // a pruned segment and a vanished buffer root look the same here.
        gate.fail(0, start);
        gate.fail(39, start);
        gate.succeed(0);
        assert!(gate.ready(0, start));
        assert!(!gate.ready(39, start));
    }
}
