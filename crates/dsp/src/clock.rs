//! Placing audio blocks on a common time axis.
//!
//! Audio backends stamp each block with the device time of its first sample, on a clock
//! shared by the microphone and the playback reference (the performance counter on Windows).
//! Stamps jitter, while the samples of one stream are contiguous, so blocks are placed back
//! to back and the clock only follows the stamps slowly, or jumps after a real gap.

/// Places consecutive blocks of one stream.
#[derive(Debug, Clone)]
pub struct BlockClock {
    rate: f64,
    /// A stamp further than this from the expected time (seconds) restarts the clock.
    reset_after: f64,
    /// Share of each stamp's error the clock follows, to track slow drift.
    follow: f64,
    next: Option<f64>,
}

/// Where a block starts, and whether the clock restarted at it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub start: f64,
    pub restarted: bool,
}

impl BlockClock {
    /// The live clock: follows drift slowly and restarts after an 80 ms jump.
    pub fn live(rate: u32) -> Self {
        Self { rate: f64::from(rate), reset_after: 0.08, follow: 0.002, next: None }
    }

    /// The recording clock: purely contiguous, restarting after a 10 ms gap. Recordings
    /// are seconds long, so drift is negligible and stamps only mark gaps.
    pub fn contiguous(rate: u32) -> Self {
        Self { rate: f64::from(rate), reset_after: 0.01, follow: 0.0, next: None }
    }

    pub fn place(&mut self, timestamp: f64, samples: usize) -> Placement {
        let (start, restarted) = match self.next {
            Some(next) if (timestamp - next).abs() <= self.reset_after => {
                (next + (timestamp - next).clamp(-0.01, 0.01) * self.follow, false)
            }
            _ => (timestamp, true),
        };
        self.next = Some(start + samples as f64 / self.rate);
        Placement { start, restarted }
    }

    /// The expected start time of the next block, if any block was placed.
    pub fn next(&self) -> Option<f64> {
        self.next
    }

    pub fn reset(&mut self) {
        self.next = None;
    }

    /// Account for a known number of lost samples without changing the clock's phase.
    pub fn skip(&mut self, samples: usize) {
        if let Some(next) = &mut self.next {
            *next += samples as f64 / self.rate;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittery_stamps_place_blocks_back_to_back() {
        let mut clock = BlockClock::contiguous(48_000);
        let first = clock.place(1.0, 480);
        let second = clock.place(1.0107, 480);
        assert!(first.restarted && !second.restarted);
        assert!((second.start - 1.01).abs() < 1e-12);
    }

    #[test]
    fn a_gap_restarts_the_clock() {
        let mut clock = BlockClock::live(48_000);
        clock.place(1.0, 480);
        let resumed = clock.place(2.0, 480);
        assert!(resumed.restarted);
        assert_eq!(resumed.start, 2.0);
    }

    #[test]
    fn a_known_lost_block_keeps_clock_phase_despite_timestamp_jitter() {
        let mut clock = BlockClock::live(48_000);
        clock.place(1.0, 480);
        clock.skip(480);
        let resumed = clock.place(1.0207, 480);
        assert!(!resumed.restarted);
        assert!((resumed.start - (1.02 + 0.0007 * 0.002)).abs() < 1e-12);
    }

    #[test]
    fn the_live_clock_follows_drift_slowly() {
        let mut clock = BlockClock::live(48_000);
        clock.place(0.0, 480);
        let placed = clock.place(0.015, 480);
        // 5 ms late, clamped to 10 ms and followed by 0.2 %.
        assert!((placed.start - (0.01 + 0.005 * 0.002)).abs() < 1e-12);
    }

    #[test]
    fn thirteen_hours_of_device_drift_do_not_accumulate_clock_error() {
        let mut clock = BlockClock::live(48_000);
        // A long-running performance counter and a device clock 50 ppm slower.
        for frame in 0..13 * 60 * 60 * 100 {
            let timestamp = 10_000_000.0 + frame as f64 * 0.0100005;
            let jitter = if frame % 2 == 0 { 0.0001 } else { -0.0001 };
            let placed = clock.place(timestamp + jitter, 480);
            assert_eq!(placed.restarted, frame == 0, "frame {frame}");
            assert!((placed.start - timestamp).abs() < 0.0004, "clock error at frame {frame}");
        }
    }
}
