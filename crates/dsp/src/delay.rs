//! Finding where the leak sits in the microphone relative to the playback reference.
//!
//! The engine lines the two streams up by their device timestamps, but Windows can report
//! the loopback and the microphone with a bias that changes while the system runs (measured
//! on one headset jack: 0.6 ms early in the morning, 19 ms early the same evening). A linear
//! canceller only models leaks that come after the reference, so a leak that appears
//! early cannot be removed at all. [`DelayEstimator`] measures the lag continuously with
//! a phase-transform cross-correlation (GCC-PHAT) on decimated audio, and reports it only
//! once several measurements agree.

use std::collections::VecDeque;

use realfft::num_complex::Complex64;

use crate::fft::RealFft;
use crate::{RATE, Stereo};

/// Audio is averaged over this many samples before correlating: 12 kHz is enough to
/// place a wideband leak within a fraction of a millisecond.
const DECIMATION: usize = 4;
/// Decimated samples correlated per measurement (about 1.4 s).
const WINDOW: usize = 16_384;
/// A measurement is taken after this many new decimated samples (0.5 s).
const EVERY: usize = 6_000;
/// Lags searched in each direction, in seconds.
const MAX_LAG: f64 = 0.1;
/// The correlation peak must stand this far above the spread of the other lags.
const PROMINENCE: f64 = 8.0;
/// Measurements that must agree, and how closely (decimated samples).
const AGREEING: usize = 3;
const TOLERANCE: i64 = 2;
/// Quieter audio than this (RMS) carries no usable leak.
const SILENCE: f64 = 1e-4;

/// A measured lag of the microphone behind the reference, in seconds. Negative when the
/// leak appears before the reference that caused it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lag {
    pub seconds: f64,
    /// How far the correlation peak stands out, as a multiple of the other lags' spread.
    pub prominence: f64,
}

#[derive(Debug)]
pub struct DelayEstimator {
    near: VecDeque<f64>,
    far: VecDeque<f64>,
    /// Partial sums for decimation.
    pending: (f64, f64, usize),
    since: usize,
    recent: VecDeque<i64>,
    fft: RealFft,
    near_spectrum: Vec<Complex64>,
    far_spectrum: Vec<Complex64>,
    correlation: Vec<f64>,
    scratch: Vec<f64>,
}

impl Default for DelayEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl DelayEstimator {
    pub fn new() -> Self {
        let fft = RealFft::new(2 * WINDOW);
        let bins = fft.bins();
        Self {
            near: VecDeque::with_capacity(WINDOW),
            far: VecDeque::with_capacity(WINDOW),
            pending: (0.0, 0.0, 0),
            since: 0,
            recent: VecDeque::with_capacity(AGREEING),
            fft,
            near_spectrum: vec![Complex64::default(); bins],
            far_spectrum: vec![Complex64::default(); bins],
            correlation: vec![0.0; 2 * WINDOW],
            scratch: Vec::with_capacity(WINDOW),
        }
    }

    /// Forget all audio and measurements, as after the alignment changed.
    pub fn clear(&mut self) {
        self.near.clear();
        self.far.clear();
        self.pending = (0.0, 0.0, 0);
        self.since = 0;
        self.recent.clear();
    }

    /// Add mono microphone samples and the reference that was aligned with them. Returns a
    /// lag when the latest measurements agree on one.
    pub fn push(&mut self, near: &[f32], far: &[Stereo]) -> Option<Lag> {
        let mut result = None;
        for (&n, &[left, right]) in near.iter().zip(far) {
            let (near_sum, far_sum, count) = &mut self.pending;
            *near_sum += f64::from(n);
            *far_sum += 0.5 * f64::from(left + right);
            *count += 1;
            if *count < DECIMATION {
                continue;
            }
            let (n, f) = (*near_sum / DECIMATION as f64, *far_sum / DECIMATION as f64);
            self.pending = (0.0, 0.0, 0);
            if self.near.len() == WINDOW {
                self.near.pop_front();
                self.far.pop_front();
            }
            self.near.push_back(n);
            self.far.push_back(f);
            self.since += 1;
            if self.near.len() == WINDOW && self.since >= EVERY {
                self.since = 0;
                result = self.measure().or(result);
            }
        }
        result
    }

    fn measure(&mut self) -> Option<Lag> {
        let rms = |x: &VecDeque<f64>| (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
        if rms(&self.near) < SILENCE || rms(&self.far) < SILENCE {
            self.recent.clear();
            return None;
        }
        self.scratch.clear();
        self.scratch.extend(self.near.iter());
        self.fft.forward(&self.scratch, &mut self.near_spectrum);
        self.scratch.clear();
        self.scratch.extend(self.far.iter());
        self.fft.forward(&self.scratch, &mut self.far_spectrum);
        for (n, f) in self.near_spectrum.iter_mut().zip(&self.far_spectrum) {
            let cross = *n * f.conj();
            let magnitude = cross.norm();
            *n = if magnitude > 1e-20 { cross / magnitude } else { Complex64::default() };
        }
        self.fft.inverse(&self.near_spectrum, &mut self.correlation);

        // Index k holds lag +k (microphone later); index size - k holds lag -k.
        let size = self.correlation.len();
        let max_lag = (MAX_LAG * f64::from(RATE) / DECIMATION as f64) as i64;
        let at = |lag: i64| self.correlation[lag.rem_euclid(size as i64) as usize];
        let (mut best, mut peak, mut sum, mut squares) = (0, f64::MIN, 0.0, 0.0);
        for lag in -max_lag..=max_lag {
            let value = at(lag);
            sum += value;
            squares += value * value;
            if value > peak {
                (best, peak) = (lag, value);
            }
        }
        let count = (2 * max_lag + 1) as f64;
        let spread = (squares / count - (sum / count).powi(2)).max(0.0).sqrt();
        let prominence = if spread > 0.0 { (peak - sum / count) / spread } else { 0.0 };
        if prominence < PROMINENCE {
            self.recent.clear();
            return None;
        }

        if self.recent.len() == AGREEING {
            self.recent.pop_front();
        }
        self.recent.push_back(best);
        let agreed = self.recent.len() == AGREEING && self.recent.iter().all(|&lag| (lag - best).abs() <= TOLERANCE);
        if !agreed {
            return None;
        }
        // Refine to a fraction of a decimated sample with a parabola through the peak.
        let (left, right) = (at(best - 1), at(best + 1));
        let curvature = left - 2.0 * peak + right;
        let offset = if curvature < 0.0 { 0.5 * (left - right) / curvature } else { 0.0 };
        let seconds = (best as f64 + offset.clamp(-0.5, 0.5)) * DECIMATION as f64 / f64::from(RATE);
        Some(Lag { seconds, prominence })
    }
}

/// The leak must come at least this long after the reference: the canceller cannot model
/// a leak earlier than its reference, and one just at it loses the start of each sound.
const EARLIEST: f64 = 0.0005;
/// The latest leak the Clean voice canceller still covers well (its filter spans 25 ms).
const LATEST: f64 = 0.02;
/// Where a realigned leak is placed.
const TARGET: f64 = 0.003;
/// The largest correction, in either direction.
const MAX_SHIFT: f64 = 0.1;

/// Keeps the leak shortly after the reference, where a canceller can model it, by moving
/// the point in the reference that each microphone frame is matched with.
#[derive(Debug, Default)]
pub struct Alignment {
    estimator: DelayEstimator,
    shift: f64,
}

impl Alignment {
    /// Seconds to add to a microphone frame's time to find its reference. Positive means the
    /// reference is read later, which delays the processed microphone by as much.
    pub fn shift(&self) -> f64 {
        self.shift
    }

    /// Measure with one frame of microphone audio and the reference read for it. Returns
    /// `true` when the shift changed, so learned echo paths no longer apply.
    pub fn update(&mut self, near: &[f32], far: &[Stereo]) -> bool {
        let Some(lag) = self.estimator.push(near, far) else { return false };
        if (EARLIEST..=LATEST).contains(&lag.seconds) {
            return false;
        }
        let shift = (self.shift + TARGET - lag.seconds).clamp(-MAX_SHIFT, MAX_SHIFT);
        if (shift - self.shift).abs() < 0.0002 {
            return false;
        }
        self.shift = shift;
        self.estimator.clear();
        true
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;
    use rand::rngs::StdRng;
    use rand_distr::{Distribution, Normal};

    use super::*;

    /// Feed `seconds` of noise playback whose leak reaches the microphone `lag` samples
    /// later (negative: earlier), with independent microphone noise.
    fn measure(lag: i64, seconds: usize) -> Option<Lag> {
        let mut rng = StdRng::seed_from_u64(7);
        let normal = Normal::new(0.0, 0.1).unwrap();
        let total = seconds * RATE as usize;
        let music: Vec<f32> = (0..total + 20_000).map(|_| normal.sample(&mut rng) as f32).collect();
        let mut estimator = DelayEstimator::new();
        let mut found = None;
        for start in (0..total).step_by(480) {
            let far: Vec<Stereo> = (start..start + 480).map(|i| [music[i + 10_000]; 2]).collect();
            let near: Vec<f32> = (start..start + 480)
                .map(|i| 0.3 * music[(i as i64 + 10_000 - lag) as usize] + 0.01 * normal.sample(&mut rng) as f32)
                .collect();
            found = estimator.push(&near, &far).or(found);
        }
        found
    }

    #[test]
    fn finds_a_leak_that_comes_after_the_reference() {
        let lag = measure(240, 4).expect("a lag");
        assert!((lag.seconds - 0.005).abs() < 0.0002, "{lag:?}");
    }

    #[test]
    fn finds_a_leak_that_comes_before_the_reference() {
        let lag = measure(-914, 4).expect("a lag");
        assert!((lag.seconds + 914.0 / 48_000.0).abs() < 0.0002, "{lag:?}");
    }

    #[test]
    fn alignment_moves_an_early_leak_after_the_reference() {
        let mut rng = StdRng::seed_from_u64(11);
        let normal = Normal::new(0.0, 0.1).unwrap();
        let music: Vec<f32> = (0..10 * RATE as usize).map(|_| normal.sample(&mut rng) as f32).collect();
        let lag = -914_i64; // the leak comes 19 ms before its reference
        let mut alignment = Alignment::default();
        let mut changes = 0;
        for start in (20_000..8 * RATE as usize).step_by(480) {
            // The reference is read `shift` later, as the engine reads it.
            let offset = (alignment.shift() * f64::from(RATE)).round() as i64;
            let far: Vec<Stereo> = (start..start + 480).map(|i| [music[(i as i64 + offset) as usize]; 2]).collect();
            let near: Vec<f32> = (start..start + 480).map(|i| 0.3 * music[(i as i64 - lag) as usize]).collect();
            changes += usize::from(alignment.update(&near, &far));
        }
        assert_eq!(changes, 1);
        assert!((alignment.shift() - (0.003 + 914.0 / 48_000.0)).abs() < 0.0003, "{}", alignment.shift());
    }

    #[test]
    fn alignment_keeps_a_leak_that_is_already_in_range() {
        let mut rng = StdRng::seed_from_u64(12);
        let normal = Normal::new(0.0, 0.1).unwrap();
        let music: Vec<f32> = (0..8 * RATE as usize).map(|_| normal.sample(&mut rng) as f32).collect();
        let mut alignment = Alignment::default();
        for start in (1_000..7 * RATE as usize).step_by(480) {
            let far: Vec<Stereo> = (start..start + 480).map(|i| [music[i]; 2]).collect();
            let near: Vec<f32> = (start..start + 480).map(|i| 0.3 * music[i - 240]).collect();
            assert!(!alignment.update(&near, &far));
        }
        assert_eq!(alignment.shift(), 0.0);
    }

    #[test]
    fn reports_nothing_without_a_leak() {
        let mut rng = StdRng::seed_from_u64(3);
        let normal = Normal::new(0.0, 0.1).unwrap();
        let mut estimator = DelayEstimator::new();
        for _ in 0..400 {
            let near: Vec<f32> = (0..480).map(|_| normal.sample(&mut rng) as f32).collect();
            let far: Vec<Stereo> = (0..480).map(|_| [normal.sample(&mut rng) as f32; 2]).collect();
            assert_eq!(estimator.push(&near, &far), None);
        }
    }

    #[test]
    fn reports_nothing_in_silence() {
        let mut estimator = DelayEstimator::new();
        for _ in 0..400 {
            assert_eq!(estimator.push(&[0.0; 480], &[[0.0; 2]; 480]), None);
        }
    }
}
