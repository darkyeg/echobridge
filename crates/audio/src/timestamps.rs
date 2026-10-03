//! Recovering invalid device timestamps without replacing healthy stamps with callback jitter.

use crate::RATE;

const FUTURE_TOLERANCE: f64 = 0.020;
const DELIVERY_MARGIN: f64 = 0.050;
// Native stamps have 100 ns precision; avoid reanchoring at a rounded boundary.
const STAMP_PRECISION: f64 = 1e-7;

#[derive(Debug, Default)]
pub(crate) struct CaptureClock {
    offset: f64,
    next: Option<f64>,
    estimating: bool,
    arrival_bias: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timestamp {
    pub time: f64,
    pub discontinuity: bool,
    pub reanchored: bool,
    pub offset: f64,
}

impl CaptureClock {
    /// `now` and a valid `reported` stamp share the performance-counter time axis.
    /// The buffer duration permits genuine queued packets; seconds in the future do
    /// not describe captured audio. Keep a recovered offset stable between packets.
    pub fn place(
        &mut self,
        reported: Option<f64>,
        now: f64,
        frames: usize,
        buffer_seconds: f64,
        discontinuity: bool,
    ) -> Timestamp {
        let duration = frames as f64 / f64::from(RATE);
        let oldest = now - (buffer_seconds.max(duration) + DELIVERY_MARGIN);
        let plausible = |time: f64| {
            time.is_finite() && time >= oldest - STAMP_PRECISION && time <= now + FUTURE_TOLERANCE + STAMP_PRECISION
        };
        let estimate = now - duration;
        let mut reanchored = false;
        let time = match reported.filter(|time| time.is_finite() && *time > 0.0) {
            Some(reported) => {
                let corrected = reported + self.offset;
                let time = if plausible(corrected) {
                    corrected
                } else if self.offset != 0.0 && plausible(reported) {
                    // A driver that returns to healthy stamps needs its old correction removed.
                    self.offset = 0.0;
                    reanchored = true;
                    reported
                } else {
                    self.offset = estimate - reported;
                    reanchored = true;
                    estimate
                };
                if self.estimating && self.next.is_some_and(|next| (time - next).abs() > 0.002) {
                    reanchored = true;
                }
                self.estimating = false;
                time
            }
            None => {
                // TIMESTAMP_ERROR leaves position/time undefined. A short bad run can
                // continue the sample clock; a real gap needs a fresh arrival anchor.
                let time = match self.next.filter(|&next| !discontinuity && plausible(next)) {
                    Some(next) => {
                        if !self.estimating {
                            self.arrival_bias = next - estimate;
                        }
                        // Track device-rate drift slowly, preserving the known delivery
                        // phase and filtering scheduler jitter during a long bad run.
                        next + (estimate + self.arrival_bias - next).clamp(-0.01, 0.01) * 0.002
                    }
                    None => {
                        self.arrival_bias = 0.0;
                        reanchored = true;
                        estimate
                    }
                };
                self.estimating = true;
                time
            }
        };
        let mapping_changed = reanchored && self.next.is_none_or(|next| (time - next).abs() > 0.002);
        self.next = Some(time + duration);
        Timestamp { time, discontinuity: discontinuity || mapping_changed, reanchored, offset: self.offset }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn healthy_stamps_keep_device_precision_despite_callback_jitter_and_backlog() {
        let mut clock = CaptureClock::default();
        for (index, age) in [0.01, 0.012, 0.075, 0.10].into_iter().enumerate() {
            let stamp = 100.0 + index as f64 * 0.01;
            let placed = clock.place(Some(stamp), stamp + age, 480, 0.1, false);
            assert_eq!(placed.time, stamp);
            assert!(!placed.discontinuity);
            assert!(!placed.reanchored);
        }
    }

    #[test]
    fn future_loopback_stamps_get_one_stable_offset_for_thirteen_hours() {
        let mut clock = CaptureClock::default();
        for frame in 0..13 * 60 * 60 * 100 {
            let now = 100_000.0 + frame as f64 * 0.0100005;
            let jitter = if frame % 2 == 0 { 0.0001 } else { -0.0001 };
            let placed = clock.place(Some(now + 3.49), now + jitter, 480, 0.1, false);
            assert_eq!(placed.reanchored, frame == 0, "frame {frame}");
            assert_eq!(placed.discontinuity, frame == 0, "frame {frame}");
            assert!((placed.time - (now - 0.0099)).abs() < 1e-9, "frame {frame}");
        }
    }

    #[test]
    fn changing_a_correction_keeps_a_continuous_mapping_without_false_discontinuities() {
        let mut clock = CaptureClock::default();
        clock.place(Some(99.99), 100.0, 480, 0.1, false);
        let jumped = clock.place(Some(103.50), 100.01, 480, 0.1, false);
        assert!(!jumped.discontinuity && jumped.reanchored);
        assert!((jumped.time - 100.0).abs() < 1e-12);
        let next = clock.place(Some(103.51), 100.02, 480, 0.1, false);
        assert!(!next.discontinuity);
        assert!((next.time - 100.01).abs() < 1e-12);
        let healthy = clock.place(Some(100.02), 100.03, 480, 0.1, false);
        assert!(!healthy.discontinuity && healthy.reanchored);
        assert_eq!(healthy.time, 100.02);
        assert_eq!(healthy.offset, 0.0);
    }

    #[test]
    fn uncertain_timestamps_continue_samples_without_callback_jitter() {
        let mut clock = CaptureClock::default();
        clock.place(Some(99.99), 100.0, 480, 0.1, false);
        for (index, jitter) in [0.0003, 0.0015, -0.0001].into_iter().enumerate() {
            let time = 100.0 + index as f64 * 0.01;
            let placed = clock.place(None, time + 0.01 + jitter, 480, 0.1, false);
            assert!((placed.time - time).abs() < 5e-6);
            assert!(!placed.discontinuity);
        }
        let recovered = clock.place(Some(100.03), 100.04, 480, 0.1, false);
        assert!(!recovered.discontinuity);
        assert!((recovered.time - 100.03).abs() < 1e-12);
    }

    #[test]
    fn uncertain_stamps_after_a_gap_reanchor_and_signal_the_changed_mapping() {
        let mut clock = CaptureClock::default();
        clock.place(Some(99.99), 100.0, 480, 0.1, false);
        let placed = clock.place(None, 102.0, 480, 0.1, true);
        assert!(placed.discontinuity && placed.reanchored);
        assert!((placed.time - 101.99).abs() < 1e-12);
        let recovered = clock.place(Some(102.005), 102.02, 480, 0.1, false);
        assert!(recovered.discontinuity);
        assert_eq!(recovered.time, 102.005);
    }

    #[test]
    fn zero_and_nonfinite_stamps_are_never_forwarded() {
        for stamp in [0.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let placed = CaptureClock::default().place(Some(stamp), 100.0, 480, 0.1, false);
            assert!(placed.discontinuity && placed.reanchored);
            assert!((placed.time - 99.99).abs() < 1e-12);
        }
    }

    #[test]
    fn a_real_mapping_jump_marks_one_discontinuity() {
        let mut clock = CaptureClock::default();
        clock.place(Some(99.99), 100.0, 480, 0.1, false);
        let jumped = clock.place(Some(103.50), 100.015, 480, 0.1, false);
        assert!(jumped.discontinuity && jumped.reanchored);
        assert!((jumped.time - 100.005).abs() < 1e-12);
        let next = clock.place(Some(103.51), 100.025, 480, 0.1, false);
        assert!(!next.discontinuity && !next.reanchored);
        assert!((next.time - 100.015).abs() < 1e-12);
    }

    #[test]
    fn thirteen_hours_of_uncertain_stamps_follow_drift_without_repeated_resets() {
        for period in [0.010001, 0.009999] {
            let mut clock = CaptureClock::default();
            clock.place(Some(99_999.99), 100_000.0, 480, 0.1, false);
            for frame in 1..13 * 60 * 60 * 100 {
                let now = 100_000.0 + frame as f64 * period;
                let jitter = if frame % 2 == 0 { 0.001 } else { -0.001 };
                let placed = clock.place(None, now + jitter, 480, 0.1, false);
                assert!(!placed.discontinuity && !placed.reanchored, "frame {frame}");
                assert!((placed.time - (now - 0.01)).abs() < 0.002, "frame {frame}");
            }
        }
    }

    #[test]
    fn backlog_at_the_buffer_margin_is_valid_but_an_impossible_age_is_corrected() {
        let mut clock = CaptureClock::default();
        let boundary = clock.place(Some(100.0), 100.15, 480, 0.1, false);
        assert_eq!(boundary.time, 100.0);
        assert!(!boundary.reanchored);
        let stale = clock.place(Some(100.01), 100.17, 480, 0.1, false);
        assert!(stale.reanchored && stale.discontinuity);
        assert!((stale.time - 100.16).abs() < 1e-12);
    }
}
