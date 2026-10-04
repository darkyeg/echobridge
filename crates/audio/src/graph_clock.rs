//! Placing PipeWire capture blocks on a steady time axis.
//!
//! A stream's `pw_time` pairs the time of the graph cycle (`now`, the moment the driver
//! woke up) with the cycle's exact sample position (`ticks`). The wake-up time jitters by
//! up to a few milliseconds on a loaded system and only ever late, while ticks never
//! jitter. Time is therefore ticks plus an offset that follows the earliest wake-ups, so
//! that blocks keep sample-exact spacing and two streams in the same graph share one axis.

/// Smoothing for an offset that has to rise, as when the audio clock runs slower than the
/// system clock. Falling is immediate: a wake-up cannot be early.
const RISE: f64 = 0.01;
/// A change this large between cycles is a clock step or a restarted graph, not jitter.
const STEP: f64 = 0.05;
/// Positions this far (seconds) from the expected next block are a gap in the data.
const GAP: f64 = 0.001;

#[derive(Debug, Default)]
pub(crate) struct GraphClock {
    offset: Option<f64>,
    next: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Placed {
    /// Time of the block's first sample on the monotonic clock, in seconds.
    pub time: f64,
    /// Samples were lost before this block.
    pub discontinuity: bool,
}

impl GraphClock {
    /// `now`: monotonic seconds at the cycle. `ticks`: the graph position at that cycle, in
    /// seconds. `delay`: how long before the cycle the device captured the block's end.
    /// `duration`: the length of the block.
    pub fn place(&mut self, now: f64, ticks: f64, delay: f64, duration: f64) -> Placed {
        let sample = now - ticks;
        let mut discontinuity = false;
        let offset = match self.offset {
            Some(offset) if (sample - offset).abs() > STEP => {
                discontinuity = true;
                sample
            }
            Some(offset) if sample < offset => sample,
            Some(offset) => offset + RISE * (sample - offset),
            None => sample,
        };
        self.offset = Some(offset);
        if self.next.is_some_and(|next| (ticks - next).abs() > GAP) {
            discontinuity = true;
        }
        self.next = Some(ticks + duration);
        Placed { time: ticks + offset - delay, discontinuity }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CYCLE: f64 = 0.0213333;

    #[test]
    fn jittery_wakeups_give_evenly_spaced_blocks() {
        let mut clock = GraphClock::default();
        let mut times = Vec::new();
        for cycle in 0..500 {
            let ticks = cycle as f64 * CYCLE;
            let jitter = [0.0, 0.0004, 0.0011, 0.0002, 0.0019][cycle % 5];
            let placed = clock.place(1000.0 + ticks + jitter, ticks, 0.0, CYCLE);
            assert!(!placed.discontinuity);
            times.push(placed.time);
        }
        // Spacing is exactly the block length; the jitter converges toward the earliest wake-up.
        for pair in times.windows(2).skip(50) {
            assert!((pair[1] - pair[0] - CYCLE).abs() < 0.0003, "{}", pair[1] - pair[0]);
        }
        assert!((times[499] - (1000.0 + 499.0 * CYCLE)).abs() < 0.0012);
    }

    #[test]
    fn lost_blocks_are_a_discontinuity() {
        let mut clock = GraphClock::default();
        clock.place(10.0, 0.0, 0.0, CYCLE);
        clock.place(10.0 + CYCLE, CYCLE, 0.0, CYCLE);
        let placed = clock.place(10.0 + 3.0 * CYCLE, 3.0 * CYCLE, 0.0, CYCLE);
        assert!(placed.discontinuity);
        assert!(!clock.place(10.0 + 4.0 * CYCLE, 4.0 * CYCLE, 0.0, CYCLE).discontinuity);
    }

    #[test]
    fn a_restarted_graph_reanchors() {
        let mut clock = GraphClock::default();
        clock.place(10.0, 5.0, 0.0, CYCLE);
        let placed = clock.place(11.0, 0.0, 0.0, CYCLE);
        assert!(placed.discontinuity);
        assert!((placed.time - 11.0).abs() < 1e-9);
    }

    #[test]
    fn device_delay_moves_the_block_back() {
        let mut clock = GraphClock::default();
        let placed = clock.place(10.0, 0.0, 0.004, CYCLE);
        assert!((placed.time - 9.996).abs() < 1e-9);
    }

    #[test]
    fn a_slow_audio_clock_is_followed() {
        let mut clock = GraphClock::default();
        // The audio clock lags 100 ppm behind the system clock.
        let mut last = 0.0;
        for cycle in 0..3000 {
            let ticks = cycle as f64 * CYCLE;
            last = clock.place(ticks * 1.0001, ticks, 0.0, CYCLE).time - ticks * 1.0001;
            assert!(last > -0.0005 && last < 0.0001, "{last}");
        }
        assert!(last.abs() < 0.0005);
    }
}
