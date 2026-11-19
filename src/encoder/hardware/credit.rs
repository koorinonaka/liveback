//! What the encoder's input credit did over a window of time (t261019-9ce9).
//!
//! `no_credit` rises at 1 s segment boundaries, and the log could not say
//! whether the encoder was busy (no `METransformNeedInput` yet) or the capture
//! loop was late picking the events up (several `NeedInput`s waiting for one
//! `poll`). This keeps the numbers that separate the two, for the
//! `task202_boundary` line.
//!
//! Plain values and explicit `Instant`s, so the arithmetic is testable without
//! an MFT.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One boundary's window of observations.
#[derive(Clone, Debug)]
pub struct CreditWindow {
    pub credit_at_open: u32,
    pub credit_min: u32,
    /// Most `NeedInput` events one `poll` picked up. Above 1 means the credit
    /// had been there before the loop came round to collect it.
    pub need_input_max: u32,
    pub poll_gap_max: Duration,
    /// The window's first submitted frame and its submit-to-output time. That
    /// frame is the forced IDR.
    first_pts: Option<i64>,
    pub first_latency: Option<Duration>,
    /// Submit-to-output of every other frame of the window that came out.
    pub other_latencies: Vec<Duration>,
    /// t261021-375d: the first frame's submit-to-output split into "until the
    /// GPU finished its conversion" and "from there to the output".
    pub first_split: Option<(Duration, Duration)>,
}

impl CreditWindow {
    fn open(credit: u32) -> Self {
        Self {
            credit_at_open: credit,
            credit_min: credit,
            need_input_max: 0,
            poll_gap_max: Duration::ZERO,
            first_pts: None,
            first_latency: None,
            other_latencies: Vec::new(),
            first_split: None,
        }
    }

    /// Median of `other_latencies`, or `None` when none came out.
    pub fn other_latency_p50(&self) -> Option<Duration> {
        let mut sorted = self.other_latencies.clone();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied()
    }

    fn on_poll(&mut self, gap: Duration, need_inputs: u32) {
        self.need_input_max = self.need_input_max.max(need_inputs);
        self.poll_gap_max = self.poll_gap_max.max(gap);
    }

    fn on_submit(&mut self, pts: i64, credit_after: u32) {
        self.credit_min = self.credit_min.min(credit_after);
        self.first_pts.get_or_insert(pts);
    }

    fn on_output(&mut self, pts: i64, latency: Duration, split: Option<(Duration, Duration)>) {
        match self.first_pts {
            Some(first) if first == pts => {
                self.first_latency = Some(latency);
                self.first_split = split;
            }
            Some(first) if pts > first => self.other_latencies.push(latency),
            // Submitted before the window opened.
            _ => {}
        }
    }
}

/// How long the credit took to climb back to its value at the boundary.
#[derive(Clone, Copy, Debug)]
struct Recovery {
    target: u32,
    since: Instant,
    took: Option<Duration>,
}

/// Inputs waiting for their output. An encoder whose output times never match
/// its inputs would otherwise grow the queue for the whole recording (120fps
/// is ~430k entries an hour); this desk runs one frame deep.
const IN_FLIGHT_MAX: usize = 64;

pub struct CreditWatch {
    /// pts, when it was submitted, and when its conversion finished.
    in_flight: VecDeque<(i64, Instant, Option<Instant>)>,
    last_poll: Instant,
    boundary: Option<CreditWindow>,
    recovery: Option<Recovery>,
}

impl CreditWatch {
    pub fn new(now: Instant) -> Self {
        Self {
            in_flight: VecDeque::new(),
            last_poll: now,
            boundary: None,
            recovery: None,
        }
    }

    /// After one `poll`: how many `NeedInput`s it picked up and the credit it
    /// left.
    pub fn on_poll(&mut self, now: Instant, need_inputs: u32, credit: u32) {
        let gap = now.saturating_duration_since(self.last_poll);
        self.last_poll = now;
        if let Some(window) = self.boundary.as_mut() {
            window.on_poll(gap, need_inputs);
        }
        if let Some(recovery) = self.recovery.as_mut() {
            if recovery.took.is_none() && credit >= recovery.target {
                recovery.took = Some(now.saturating_duration_since(recovery.since));
            }
        }
    }

    pub fn on_submit(&mut self, now: Instant, pts: i64, credit_after: u32) {
        if self.in_flight.len() == IN_FLIGHT_MAX {
            self.in_flight.pop_front();
        }
        self.in_flight.push_back((pts, now, None));
        if let Some(window) = self.boundary.as_mut() {
            window.on_submit(pts, credit_after);
        }
    }

    /// One output sample carrying `pts`. Inputs it overtook (never output,
    /// or output out of order) are dropped from the queue unmeasured.
    pub fn on_output(&mut self, now: Instant, pts: i64) {
        while let Some(&(queued, submitted, converted)) = self.in_flight.front() {
            if queued > pts {
                break;
            }
            self.in_flight.pop_front();
            if queued == pts {
                let latency = now.saturating_duration_since(submitted);
                // The conversion is queued before the submit, so it can be
                // seen finishing "before" it: that wait is 0.
                let split = converted.map(|done| {
                    (
                        done.saturating_duration_since(submitted),
                        now.saturating_duration_since(done),
                    )
                });
                if let Some(window) = self.boundary.as_mut() {
                    window.on_output(pts, latency, split);
                }
            }
        }
    }

    /// The GPU finished converting the frame submitted at `pts`
    /// (t261021-375d). Seen from the pump, so `now` is late by up to one pump.
    pub fn on_convert_done(&mut self, pts: i64, now: Instant) {
        if let Some(entry) = self.in_flight.iter_mut().find(|entry| entry.0 == pts) {
            entry.2.get_or_insert(now);
        }
    }

    /// Opens a boundary window with `credit` as it stood before the boundary's
    /// frame is submitted. Returns how long the previous boundary's credit
    /// took to recover: `Some(None)` if it never did, `None` if there was no
    /// previous boundary.
    pub fn open_boundary(&mut self, now: Instant, credit: u32) -> Option<Option<Duration>> {
        let previous = self.recovery.map(|recovery| recovery.took);
        self.boundary = Some(CreditWindow::open(credit));
        self.recovery = Some(Recovery {
            target: credit,
            since: now,
            took: None,
        });
        previous
    }

    /// Closes the boundary window, with the recovery time if it is known yet.
    pub fn close_boundary(&mut self) -> Option<(CreditWindow, Option<Duration>)> {
        let window = self.boundary.take()?;
        Some((window, self.recovery.and_then(|recovery| recovery.took)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A boundary on a busy encoder: credit drains to 0, two `NeedInput`s pile
    /// up behind a 51ms poll gap, the IDR comes out slower than the P frames,
    /// and the credit is back 60ms after the boundary.
    #[test]
    fn a_boundary_window_reports_min_gap_latencies_and_recovery() {
        let t0 = Instant::now();
        let mut watch = CreditWatch::new(t0);
        watch.on_poll(t0, 0, 2);
        assert_eq!(watch.open_boundary(t0, 2), None);
        watch.on_submit(t0, 1000, 1); // the IDR
        watch.on_poll(t0 + ms(5), 0, 1);
        watch.on_submit(t0 + ms(16), 1166, 0);
        watch.on_poll(t0 + ms(56), 2, 2); // 51ms gap, 2 NeedInputs
        watch.on_output(t0 + ms(56), 1000);
        watch.on_output(t0 + ms(60), 1166);
        watch.on_submit(t0 + ms(60), 1500, 1);
        watch.on_output(t0 + ms(70), 1500);

        let (window, recovery) = watch.close_boundary().expect("open");
        assert_eq!(window.credit_at_open, 2);
        assert_eq!(window.credit_min, 0);
        assert_eq!(window.need_input_max, 2);
        assert_eq!(window.poll_gap_max, ms(51));
        assert_eq!(window.first_latency, Some(ms(56)));
        assert_eq!(window.other_latencies, vec![ms(44), ms(10)]);
        assert_eq!(window.other_latency_p50(), Some(ms(44)));
        assert_eq!(recovery, Some(ms(56)));
    }

    /// The window closes before the credit is back: this line says unknown,
    /// the next boundary's line carries the answer.
    #[test]
    fn a_recovery_slower_than_the_window_reaches_the_next_boundary() {
        let t0 = Instant::now();
        let mut watch = CreditWatch::new(t0);
        watch.open_boundary(t0, 3);
        watch.on_submit(t0, 10, 2);
        watch.on_poll(t0 + ms(400), 0, 2);
        assert_eq!(watch.close_boundary().expect("open").1, None);
        watch.on_poll(t0 + ms(700), 1, 3);
        assert_eq!(watch.open_boundary(t0 + ms(1000), 3), Some(Some(ms(700))));
        assert_eq!(watch.open_boundary(t0 + ms(2000), 3), Some(None));
    }

    /// Output that skipped an input: the skipped one is dropped, not matched
    /// to the later sample's time.
    #[test]
    fn an_output_drops_the_inputs_it_overtook() {
        let t0 = Instant::now();
        let mut watch = CreditWatch::new(t0);
        watch.open_boundary(t0, 1);
        watch.on_submit(t0, 10, 0);
        watch.on_submit(t0 + ms(10), 20, 0);
        watch.on_output(t0 + ms(30), 20);
        let (window, _) = watch.close_boundary().expect("open");
        assert_eq!(window.first_latency, None);
        assert_eq!(window.other_latencies, vec![ms(20)]);
    }

    /// t261021-375d: the conversion finishing splits the IDR's submit-to-output;
    /// a conversion seen "before" the submit is a 0 wait, and one never seen
    /// leaves the split unknown rather than 0.
    #[test]
    fn a_seen_conversion_splits_the_idr_latency() {
        let t0 = Instant::now();
        let mut watch = CreditWatch::new(t0);
        watch.open_boundary(t0, 1);
        watch.on_submit(t0, 10, 0); // the IDR
        watch.on_convert_done(10, t0 + ms(30));
        watch.on_output(t0 + ms(33), 10);
        let (window, _) = watch.close_boundary().expect("open");
        assert_eq!(window.first_split, Some((ms(30), ms(3))));

        watch.open_boundary(t0 + ms(50), 1);
        watch.on_submit(t0 + ms(50), 30, 0);
        watch.on_convert_done(30, t0 + ms(49));
        watch.on_output(t0 + ms(52), 30);
        let (window, _) = watch.close_boundary().expect("open");
        assert_eq!(window.first_split, Some((ms(0), ms(3))));

        watch.open_boundary(t0 + ms(60), 1);
        watch.on_submit(t0 + ms(60), 40, 0);
        watch.on_output(t0 + ms(68), 40); // conversion never seen
        let (window, _) = watch.close_boundary().expect("open");
        assert_eq!(window.first_latency, Some(ms(8)));
        assert_eq!(window.first_split, None);
    }

    /// Outputs that never carry an input's time: the queue stops growing.
    #[test]
    fn unmatched_inputs_are_bounded() {
        let t0 = Instant::now();
        let mut watch = CreditWatch::new(t0);
        for pts in 0..1000 {
            watch.on_submit(t0, pts * 10, 0);
            watch.on_output(t0, -1);
        }
        assert_eq!(watch.in_flight.len(), IN_FLIGHT_MAX);
    }
}
