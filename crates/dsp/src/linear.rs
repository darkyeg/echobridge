//! Full-band linear echo cancellation for electrical (and other linear) playback leaks.
//!
//! WebRTC AEC3 subtracts a linear echo estimate only below 8 kHz and then shapes the rest
//! with suppression gains. On a leak that is linear across the whole spectrum, as in a
//! headset combo jack, those gains audibly reshape the voice while it overlaps playback.
//! This canceller subtracts the predicted leak at every frequency and never applies a gain
//! to the microphone, so speech passes unchanged.
//!
//! It is a partitioned-block frequency-domain adaptive filter (overlap-save) with one
//! filter per reference channel. A fast background filter adapts continuously; the
//! foreground filter that produces the output changes only when a change is proven, so
//! speech cannot reach the output through adaptation.
//!
//! Proof matters because a voice can be correlated with the playback: a user singing along
//! has the song's notes, and a fast filter cancels those harmonics as if they were leak
//! (measured: up to 16 dB of the voice removed, and the song back afterwards). The rules,
//! measured in docs/AUDIO-DIAGNOSIS.md:
//!
//! - The foreground filter "explains" the microphone when what it leaves is at least 10 dB
//!   below the leak it predicts. A voice it cannot predict breaks this, even a singing one.
//! - While it explains the microphone, snapshots of the background filter that cancel
//!   better are tested, frozen, on the following audio and adopted only if they still
//!   cancel better there. A filter that chased a voice only works on the notes it learned on.
//! - While it does not (a voice, or a new leak path), only two changes are accepted: a new
//!   level of the same path, which no voice can imitate, or a new shape that holds for 5 s.
//! - When the foreground filter explains the microphone again after a voice, the background
//!   filter restarts from it. What it learned during the voice sits in frequencies the
//!   music is not playing now, where no error can reveal it until the music moves there.

use realfft::num_complex::Complex64;

use crate::DspError;
use crate::fft::RealFft;

// Trial lengths in blocks (5 ms at the default block size).
/// Before the first convergence.
const FIRST_TRIAL: u32 = 40;
/// While the foreground filter explains the microphone.
const REFINE_TRIAL: u32 = 200;
/// A new level of the known path.
const GAIN_TRIAL: u32 = 100;
/// A new path shape, while something is unexplained.
const SHAPE_TRIAL: u32 = 1000;
/// Unexplained-to-explained energy ratio: below 0.1 (-10 dB) the foreground filter explains
/// the microphone; it stops only above 0.3 (-5 dB), so the state does not flicker.
const EXPLAINED: f64 = 0.1;
const UNEXPLAINED: f64 = 0.3;
/// Trials start after 0.5 s of quiet and count blocks only after 0.1 s of it.
const SETTLED_START: u32 = 100;
const SETTLED_COUNT: u32 = 20;
/// A level trial starts when the best level differs from the current one by 5 %.
const LEVEL_CHANGE: f64 = 0.05;
/// A candidate must leave less than 70 % of the current filter's residual energy.
const IMPROVEMENT: f64 = 0.7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LinearConfig {
    /// Samples per block; the filter adapts once per block.
    pub block: usize,
    /// Blocks of history the filter spans: `block * partitions` taps.
    pub partitions: usize,
    /// Reference channels, one filter each.
    pub channels: usize,
    /// Normalized adaptation step of the background filter.
    pub step: f64,
}

impl Default for LinearConfig {
    /// 1200 taps (25 ms) over a stereo reference.
    fn default() -> Self {
        Self { block: 240, partitions: 5, channels: 2, step: 0.5 }
    }
}

impl LinearConfig {
    pub fn taps(&self) -> usize {
        self.block * self.partitions
    }
}

/// A frozen candidate filter being compared with the foreground on new audio.
#[derive(Debug)]
struct Trial {
    filter: Vec<Complex64>,
    /// Residual energy left by the candidate, and by the foreground, over counted blocks.
    candidate_energy: f64,
    foreground_energy: f64,
    blocks: u32,
    age: u32,
    length: u32,
}

/// Per-block working buffers, kept to avoid allocating on the audio thread.
#[derive(Debug)]
struct Scratch {
    time: Vec<f64>,
    spectrum: Vec<Complex64>,
    echo_spectrum: Vec<Complex64>,
    error_spectrum: Vec<Complex64>,
    output_spectrum: Vec<Complex64>,
    gain: Vec<f64>,
    near: Vec<f64>,
    output: Vec<f64>,
    error: Vec<f64>,
    trial: Vec<f64>,
}

#[derive(Debug)]
pub struct LinearCanceller {
    config: LinearConfig,
    bins: usize,
    fft: RealFft,
    /// Reference spectra, `[partition][channel][bin]`; partition 0 is the newest block.
    spectra: Vec<Complex64>,
    background: Vec<Complex64>,
    foreground: Vec<Complex64>,
    /// The previous reference block per channel, `[channel][sample]`.
    previous: Vec<f64>,
    power: Vec<f64>,
    echo_power: Vec<f64>,
    residual_power: Vec<f64>,
    /// Smoothed residual energies of the microphone, foreground, and background filters.
    energy: [f64; 3],
    /// Smoothed left-over and predicted energies of the foreground and background filters.
    fit: [f64; 4],
    converged: bool,
    explained: bool,
    quiet_blocks: u32,
    try_level: bool,
    trial: Option<Trial>,
    adoptions: u64,
    scratch: Scratch,
}

impl LinearCanceller {
    pub fn new(config: LinearConfig) -> Self {
        assert!(config.block > 0 && config.partitions > 0 && config.channels > 0);
        let fft = RealFft::new(2 * config.block);
        let bins = fft.bins();
        let filter = vec![Complex64::default(); config.partitions * config.channels * bins];
        let b = config.block;
        Self {
            config,
            bins,
            fft,
            spectra: filter.clone(),
            background: filter.clone(),
            foreground: filter,
            previous: vec![0.0; config.channels * b],
            power: vec![0.0; bins],
            echo_power: vec![0.0; bins],
            residual_power: vec![0.0; bins],
            energy: [0.0; 3],
            fit: [0.0; 4],
            converged: false,
            explained: false,
            quiet_blocks: 0,
            try_level: true,
            trial: None,
            adoptions: 0,
            scratch: Scratch {
                time: vec![0.0; 2 * b],
                spectrum: vec![Complex64::default(); bins],
                echo_spectrum: vec![Complex64::default(); bins],
                error_spectrum: vec![Complex64::default(); bins],
                output_spectrum: vec![Complex64::default(); bins],
                gain: vec![0.0; bins],
                near: vec![0.0; b],
                output: vec![0.0; b],
                error: vec![0.0; b],
                trial: vec![0.0; b],
            },
        }
    }

    pub fn config(&self) -> LinearConfig {
        self.config
    }

    /// How many times the output filter has changed.
    pub fn adoptions(&self) -> u64 {
        self.adoptions
    }

    /// Whether the output filter currently predicts the microphone well.
    pub fn explained(&self) -> bool {
        self.explained
    }

    /// Forget everything learned, as after a gap in the audio.
    pub fn reset(&mut self) {
        *self = Self::new(self.config);
    }

    /// Cancel the leak in mono `near` using interleaved `far` with `channels` channels.
    ///
    /// `near.len()` must be whole blocks, and `far` must hold the same number of frames.
    pub fn process(&mut self, near: &[f32], far: &[f32], out: &mut [f32]) -> Result<(), DspError> {
        let LinearConfig { block, channels, .. } = self.config;
        if !near.len().is_multiple_of(block) || far.len() != near.len() * channels || out.len() != near.len() {
            return Err(DspError::BlockMismatch { block });
        }
        for ((near, far), out) in
            near.chunks_exact(block).zip(far.chunks_exact(block * channels)).zip(out.chunks_exact_mut(block))
        {
            for (n, &s) in self.scratch.near.iter_mut().zip(near) {
                *n = f64::from(s);
            }
            self.process_block(far);
            for (o, &s) in out.iter_mut().zip(&self.scratch.output) {
                *o = s as f32;
            }
        }
        Ok(())
    }

    fn process_block(&mut self, far: &[f32]) {
        let LinearConfig { block: b, channels: c, .. } = self.config;
        let bins = self.bins;
        // Shift the reference history by one partition and transform the new block,
        // overlapped with the previous one.
        self.spectra.rotate_right(c * bins);
        for channel in 0..c {
            let previous = &mut self.previous[channel * b..(channel + 1) * b];
            self.scratch.time[..b].copy_from_slice(previous);
            for (i, sample) in previous.iter_mut().enumerate() {
                *sample = f64::from(far[i * c + channel]);
            }
            self.scratch.time[b..].copy_from_slice(previous);
            self.fft.forward(&self.scratch.time, &mut self.spectra[channel * bins..(channel + 1) * bins]);
        }

        let s = &mut self.scratch;
        residual(
            &mut self.fft,
            &self.spectra,
            &self.foreground,
            bins,
            &s.near,
            &mut s.time,
            &mut s.echo_spectrum,
            &mut s.output,
        );
        residual(
            &mut self.fft,
            &self.spectra,
            &self.background,
            bins,
            &s.near,
            &mut s.time,
            &mut s.spectrum,
            &mut s.error,
        );
        self.adapt();

        let s = &self.scratch;
        let energies = [dot(&s.near, &s.near), dot(&s.output, &s.output), dot(&s.error, &s.error)];
        for (e, new) in self.energy.iter_mut().zip(energies) {
            *e = 0.8 * *e + 0.2 * new;
        }
        let [microphone, foreground, background] = self.energy;
        let fits =
            [energies[1], difference_energy(&s.near, &s.output), energies[2], difference_energy(&s.near, &s.error)];
        for (f, new) in self.fit.iter_mut().zip(fits) {
            *f = 0.98 * *f + 0.02 * new;
        }
        let limit = if self.explained { UNEXPLAINED } else { EXPLAINED };
        let explained = self.fit[0] < limit * self.fit[1];
        let background_explains = self.fit[2] < EXPLAINED * self.fit[3];
        if explained && !self.explained {
            self.background.copy_from_slice(&self.foreground);
        }
        self.explained = explained;
        self.converged |= explained;
        self.quiet_blocks = if explained || background_explains { self.quiet_blocks + 1 } else { 0 };

        if self.trial.is_some() {
            self.run_trial();
        }
        if self.trial.is_none() && self.quiet_blocks >= SETTLED_START {
            let better = background < IMPROVEMENT * foreground && background < microphone;
            if !self.converged || explained {
                if better {
                    let length = if self.converged { REFINE_TRIAL } else { FIRST_TRIAL };
                    self.start_trial(self.background.clone(), length);
                }
            } else {
                let reference: f64 = self.foreground.iter().map(|w| w.norm_sqr()).sum();
                let shared: f64 = self.foreground.iter().zip(&self.background).map(|(f, b)| (f.conj() * b).re).sum();
                let level = shared / (reference + 1e-30);
                if self.try_level && (level - 1.0).abs() > LEVEL_CHANGE {
                    let scaled = self.foreground.iter().map(|w| w * level).collect();
                    self.start_trial(scaled, GAIN_TRIAL);
                } else if better {
                    self.start_trial(self.background.clone(), SHAPE_TRIAL);
                }
            }
        }
        if background > 4.0 * foreground.max(microphone) {
            // The background filter diverged; restart it from the output filter.
            self.background.copy_from_slice(&self.foreground);
        }
    }

    /// One normalized least-mean-squares step of the background filter, per frequency bin
    /// over all reference channels.
    fn adapt(&mut self) {
        let LinearConfig { block: b, partitions, step, .. } = self.config;
        let bins = self.bins;
        let s = &mut self.scratch;
        for (k, power) in self.power.iter_mut().enumerate() {
            let current: f64 = self.spectra[..self.config.channels * bins]
                .chunks_exact(bins)
                .map(|channel| channel[k].norm_sqr())
                .sum();
            *power = 0.9 * *power + 0.1 * current;
        }
        let mean_power = self.power.iter().sum::<f64>() / bins as f64;
        let floor = 1e-10 * 2.0 * b as f64 + 1e-3 * mean_power;
        for (g, p) in s.gain.iter_mut().zip(&self.power) {
            *g = step / (partitions as f64 * p + floor);
        }
        spectrum_of_block(&mut self.fft, &s.error, &mut s.time, &mut s.error_spectrum);
        spectrum_of_block(&mut self.fft, &s.output, &mut s.time, &mut s.output_spectrum);
        for k in 0..bins {
            self.echo_power[k] = 0.5 * self.echo_power[k] + 0.5 * s.echo_spectrum[k].norm_sqr();
            self.residual_power[k] = 0.5 * self.residual_power[k] + 0.5 * s.output_spectrum[k].norm_sqr();
        }
        // Once converged, slow adaptation in bins where the residual is not leak but the
        // user's voice: the predicted-leak share of each bin bounds a useful step.
        let [microphone, foreground, _] = self.energy;
        if foreground < 0.1 * microphone {
            for ((gain, &echo), &residual) in s.gain.iter_mut().zip(&self.echo_power).zip(&self.residual_power) {
                *gain *= echo / (echo + residual + 1e-20);
            }
        }
        for (filter, spectra) in self.background.chunks_exact_mut(bins).zip(self.spectra.chunks_exact(bins)) {
            let step = s.gain.iter().zip(&s.error_spectrum);
            for ((out, x), (&gain, &error)) in s.spectrum.iter_mut().zip(spectra).zip(step) {
                *out = x.conj() * (gain * error);
            }
            // Constrain to a linear (not circular) convolution: keep the first half in time.
            self.fft.inverse(&s.spectrum, &mut s.time);
            s.time[b..].fill(0.0);
            self.fft.forward(&s.time, &mut s.spectrum);
            for (w, g) in filter.iter_mut().zip(&s.spectrum) {
                *w += g;
            }
        }
    }

    fn start_trial(&mut self, filter: Vec<Complex64>, length: u32) {
        self.trial = Some(Trial { filter, candidate_energy: 0.0, foreground_energy: 0.0, blocks: 0, age: 0, length });
    }

    fn run_trial(&mut self) {
        let Some(trial) = self.trial.as_mut() else { return };
        trial.age += 1;
        if trial.age > 3 * trial.length {
            self.trial = None;
            return;
        }
        if self.quiet_blocks < SETTLED_COUNT {
            return; // a voice pauses a trial but cannot pass it
        }
        let s = &mut self.scratch;
        residual(
            &mut self.fft,
            &self.spectra,
            &trial.filter,
            self.bins,
            &s.near,
            &mut s.time,
            &mut s.spectrum,
            &mut s.trial,
        );
        trial.candidate_energy += dot(&s.trial, &s.trial);
        trial.foreground_energy += dot(&s.output, &s.output);
        trial.blocks += 1;
        if trial.blocks < trial.length {
            return;
        }
        let trial = self.trial.take().expect("checked above");
        if trial.candidate_energy < IMPROVEMENT * trial.foreground_energy {
            self.foreground = trial.filter;
            self.adoptions += 1;
            self.try_level = true;
        } else {
            self.try_level = trial.length != GAIN_TRIAL;
        }
    }
}

/// `near` minus the leak `weights` predict from `spectra`; also returns the prediction's
/// spectrum.
#[allow(clippy::too_many_arguments)]
fn residual(
    fft: &mut RealFft,
    spectra: &[Complex64],
    weights: &[Complex64],
    bins: usize,
    near: &[f64],
    time: &mut [f64],
    spectrum: &mut [Complex64],
    out: &mut [f64],
) {
    spectrum.fill(Complex64::default());
    for (w, x) in weights.chunks_exact(bins).zip(spectra.chunks_exact(bins)) {
        for k in 0..bins {
            spectrum[k] += w[k] * x[k];
        }
    }
    fft.inverse(spectrum, time);
    let b = near.len();
    for ((o, n), e) in out.iter_mut().zip(near).zip(&time[b..]) {
        *o = n - e;
    }
}

/// The spectrum of a block placed in the second half of a zeroed overlap-save window.
fn spectrum_of_block(fft: &mut RealFft, block: &[f64], time: &mut [f64], out: &mut [Complex64]) {
    let b = block.len();
    time[..b].fill(0.0);
    time[b..].copy_from_slice(block);
    fft.forward(time, out);
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn difference_energy(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_blocks_are_rejected() {
        let mut canceller = LinearCanceller::new(LinearConfig::default());
        let mut out = vec![0.0; 100];
        assert!(canceller.process(&[0.0; 100], &[0.0; 200], &mut out).is_err());
    }

    #[test]
    fn silence_stays_silent() {
        let mut canceller = LinearCanceller::new(LinearConfig::default());
        let mut out = vec![1.0; 480];
        canceller.process(&[0.0; 480], &[0.0; 960], &mut out).unwrap();
        assert!(out.iter().all(|&s| s == 0.0));
    }
}
