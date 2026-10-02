//! The playback reference on the microphone's time axis.
//!
//! Playback blocks arrive with their own stamps; the engine asks for the reference that
//! played during each microphone frame. Samples are interpolated at the exact times, so
//! the two streams line up to a fraction of a sample.

use std::collections::VecDeque;

use crate::Stereo;
use crate::clock::BlockClock;

/// How much playback is kept, in seconds.
const HISTORY: f64 = 1.0;
/// Blocks within this margin (seconds) of a requested frame are considered for it.
const MARGIN: f64 = 0.01;

#[derive(Debug)]
struct Block {
    start: f64,
    samples: Vec<Stereo>,
}

/// Recent playback, placed by a live [`BlockClock`].
#[derive(Debug)]
pub struct ReferenceTimeline {
    rate: f64,
    clock: BlockClock,
    blocks: VecDeque<Block>,
    // Reused by `frame`: every sample time and value of the overlapping blocks.
    points: Vec<(f64, Stereo)>,
}

impl ReferenceTimeline {
    pub fn new(rate: u32) -> Self {
        Self { rate: f64::from(rate), clock: BlockClock::live(rate), blocks: VecDeque::new(), points: Vec::new() }
    }

    /// Add a playback block whose first sample played at `timestamp` seconds.
    pub fn append(&mut self, samples: &[Stereo], timestamp: f64) {
        let placed = self.clock.place(timestamp, samples.len());
        if placed.restarted {
            // A jump in time: what came before belongs to another timeline.
            self.blocks.clear();
        }
        self.blocks.push_back(Block { start: placed.start, samples: samples.to_vec() });
        let end = self.end().unwrap_or(placed.start);
        while self.blocks.front().is_some_and(|b| b.start < end - HISTORY) {
            self.blocks.pop_front();
        }
    }

    /// The time just after the newest sample, if there is any playback.
    pub fn end(&self) -> Option<f64> {
        self.clock.next()
    }

    /// Whether playback up to `time` has arrived.
    pub fn covers(&self, time: f64) -> bool {
        self.end().is_some_and(|end| end >= time)
    }

    /// Fill `out` with playback from `start` seconds on, one sample per `1 / rate`.
    ///
    /// Samples with no playback around them are silent. Returns the share of `out` that
    /// playback covered, from 0 to 1.
    pub fn frame(&mut self, start: f64, out: &mut [Stereo]) -> f64 {
        out.fill([0.0; 2]);
        if out.is_empty() {
            return 1.0;
        }
        let period = 1.0 / self.rate;
        let first = start;
        let last = start + (out.len() - 1) as f64 * period;
        self.points.clear();
        for block in &self.blocks {
            let block_end = block.start + block.samples.len() as f64 * period;
            if block_end > first - MARGIN && block.start < last + MARGIN {
                self.points
                    .extend(block.samples.iter().enumerate().map(|(i, &s)| (block.start + i as f64 * period, s)));
            }
        }
        if self.points.is_empty() {
            return 0.0;
        }
        // Slow clock corrections can make neighbouring blocks overlap by a fraction of a
        // sample; a stable sort keeps arrival order among equal times.
        self.points.sort_by(|a, b| a.0.total_cmp(&b.0));
        interpolate(&self.points, start, period, out)
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.clock.reset();
    }
}

/// Linear interpolation like `numpy.interp` with zero outside the points. Returns the
/// share of output times inside the points' span.
fn interpolate(points: &[(f64, Stereo)], start: f64, period: f64, out: &mut [Stereo]) -> f64 {
    let (first_time, last_time) = (points[0].0, points[points.len() - 1].0);
    let mut covered = 0usize;
    let mut j = 0usize;
    for (i, sample) in out.iter_mut().enumerate() {
        let t = start + i as f64 * period;
        if t < first_time || t > last_time {
            continue;
        }
        covered += 1;
        while j + 1 < points.len() && points[j + 1].0 <= t {
            j += 1;
        }
        if j + 1 == points.len() {
            *sample = points[j].1;
            continue;
        }
        let (t0, a) = points[j];
        let (t1, b) = points[j + 1];
        let weight = ((t - t0) / (t1 - t0)) as f32;
        *sample = [a[0] + (b[0] - a[0]) * weight, a[1] + (b[1] - a[1]) * weight];
    }
    covered as f64 / out.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(from: usize, count: usize) -> Vec<Stereo> {
        (from..from + count).map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn returns_the_samples_played_at_the_requested_time() {
        let mut timeline = ReferenceTimeline::new(48_000);
        timeline.append(&ramp(0, 480), 10.0);
        timeline.append(&ramp(480, 480), 10.01);
        let mut out = vec![[0.0; 2]; 480];
        let coverage = timeline.frame(10.0 + 240.0 / 48_000.0, &mut out);
        assert_eq!(coverage, 1.0);
        for (i, s) in out.iter().enumerate() {
            assert!((s[0] - (240 + i) as f32).abs() < 1e-3, "sample {i}: {s:?}");
        }
    }

    #[test]
    fn interpolates_between_samples() {
        let mut timeline = ReferenceTimeline::new(48_000);
        timeline.append(&ramp(0, 480), 0.0);
        let mut out = vec![[0.0; 2]; 4];
        timeline.frame(0.5 / 48_000.0, &mut out);
        assert!((out[0][0] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn missing_playback_is_silent_and_reported() {
        let mut timeline = ReferenceTimeline::new(48_000);
        timeline.append(&ramp(1, 240), 0.0);
        let mut out = vec![[0.0; 2]; 480];
        let coverage = timeline.frame(0.0, &mut out);
        assert!((coverage - 0.5).abs() < 0.01);
        assert_eq!(out[479], [0.0, 0.0]);
        let mut empty = ReferenceTimeline::new(48_000);
        assert_eq!(empty.frame(0.0, &mut out), 0.0);
    }

    #[test]
    fn a_jump_in_time_drops_older_playback() {
        let mut timeline = ReferenceTimeline::new(48_000);
        timeline.append(&ramp(0, 480), 0.0);
        timeline.append(&ramp(0, 480), 5.0);
        let mut out = vec![[0.0; 2]; 480];
        assert_eq!(timeline.frame(0.0, &mut out), 0.0);
        assert!(timeline.covers(5.01) && !timeline.covers(5.02));
    }

    #[test]
    fn keeps_one_second_of_history() {
        let mut timeline = ReferenceTimeline::new(48_000);
        for block in 0..300 {
            timeline.append(&ramp(0, 480), block as f64 * 0.01);
        }
        assert!(timeline.blocks.len() <= 101);
    }
}
