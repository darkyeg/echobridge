//! DeepFilterNet3 speech enhancement (MIT/Apache-2.0), run on the caller's audio thread.
//!
//! The model processes one 10 ms hop per call (about 1.5 ms of CPU on the test PC) and
//! delays audio by 30 ms. DeepFilterNet's own LADSPA plugin was rejected: its polling
//! worker thread sometimes missed a hop, and the plugin then inserted 10 ms of silence and
//! kept the extra delay for the rest of the call. Here a slow hop can only be late.
//!
//! Measured on the user's real microphone hiss with a known voice (docs/AUDIO-DIAGNOSIS.md),
//! it removed about 20 dB of hiss and kept speech within 0.3 dB, where WebRTC noise
//! suppression removed less and damaged the voice.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use df::tract::{DfParams, DfTract, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};

/// Samples per hop at 48 kHz.
pub const FRAME: usize = 480;
/// Model delay in samples (STFT overlap plus two frames of look-ahead), measured.
pub const DELAY: usize = 1440;
/// A hop must finish well inside its 10 ms.
const SLOW_HOP: Duration = Duration::from_millis(8);
/// After this many slow hops in a row the model stops and audio passes through with the
/// same delay, so timing does not jump.
const MAX_SLOW_HOPS: u32 = 20;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the AI noise suppression model could not start: {0}")]
    Model(String),
    #[error("unexpected AI noise suppression hop of {0} samples")]
    HopSize(usize),
}

/// Model thresholds, in dB of local signal-to-noise ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Most the model may attenuate. A limit keeps some room tone, so the voice never sits
    /// on dead silence.
    pub attenuation_limit_db: f32,
    /// Frames below this are treated as noise only.
    pub min_snr_db: f32,
    /// Frames above these skip the matching stage as already clean.
    pub max_erb_snr_db: f32,
    pub max_df_snr_db: f32,
    pub post_filter_beta: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            attenuation_limit_db: 24.0,
            min_snr_db: -15.0,
            max_erb_snr_db: 35.0,
            max_df_snr_db: 35.0,
            post_filter_beta: 0.0,
        }
    }
}

pub struct Denoiser {
    model: DfTract,
    /// Raw input over the model delay, the pass-through signal after an overload.
    history: VecDeque<[f32; FRAME]>,
    slow_hops: u32,
    overloaded: bool,
}

// SAFETY: `DfTract` is not `Send` only because tract keeps intermediate tensors in `Rc`
// and operator states in boxed trait objects. All of them are created by and owned
// exclusively by this one model (it is never cloned; `Denoiser` is not `Clone`), and the
// model's shared weights are behind `Arc`. Moving the whole model to another thread
// therefore cannot race on any reference count. This lets the 0.3 s model load happen off
// the audio thread. It is still used from one thread at a time (`process` takes `&mut`).
unsafe impl Send for Denoiser {}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser").field("overloaded", &self.overloaded).finish_non_exhaustive()
    }
}

impl Denoiser {
    /// Load the model (about 0.3 s), at 48 kHz mono.
    pub fn new(settings: Settings) -> Result<Self, Error> {
        let params = RuntimeParams::default_with_ch(1)
            .with_atten_lim(settings.attenuation_limit_db)
            .with_thresholds(settings.min_snr_db, settings.max_erb_snr_db, settings.max_df_snr_db)
            .with_post_filter(settings.post_filter_beta.max(0.0));
        let model = catch_unwind(|| DfTract::new(DfParams::default(), &params))
            .map_err(|_| Error::Model("the model panicked while loading".into()))?
            .map_err(|error| Error::Model(error.to_string()))?;
        if model.hop_size != FRAME {
            return Err(Error::HopSize(model.hop_size));
        }
        Ok(Self {
            model,
            history: std::iter::repeat_n([0.0; FRAME], DELAY / FRAME).collect(),
            slow_hops: 0,
            overloaded: false,
        })
    }

    /// Whether the CPU could not keep up and audio now passes through unenhanced.
    pub fn overloaded(&self) -> bool {
        self.overloaded
    }

    /// Enhance one 10 ms mono hop; the result is delayed by [`DELAY`] samples.
    pub fn process(&mut self, input: &[f32; FRAME], out: &mut [f32; FRAME]) {
        self.history.push_back(*input);
        let delayed = self.history.pop_front().expect("history holds the delay");
        if self.overloaded {
            *out = delayed;
            return;
        }
        let started = Instant::now();
        let noisy = ArrayView2::from_shape((1, FRAME), input).expect("one hop");
        let enhanced = ArrayViewMut2::from_shape((1, FRAME), out).expect("one hop");
        let model = &mut self.model;
        let result = catch_unwind(AssertUnwindSafe(|| model.process(noisy, enhanced)));
        if !matches!(result, Ok(Ok(snr)) if !snr.is_nan()) {
            self.overloaded = true;
            *out = delayed;
            return;
        }
        if started.elapsed() > SLOW_HOP {
            self.slow_hops += 1;
            self.overloaded = self.slow_hops >= MAX_SLOW_HOPS;
        } else {
            self.slow_hops = 0;
        }
    }

    #[cfg(test)]
    fn force_overload(&mut self) {
        self.overloaded = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overload_passes_audio_through_with_the_same_delay() {
        let mut denoiser = Denoiser::new(Settings::default()).unwrap();
        denoiser.force_overload();
        let ramp: Vec<f32> = (0..FRAME * 10).map(|i| i as f32).collect();
        let mut output = Vec::new();
        let mut out = [0.0; FRAME];
        for hop in ramp.as_chunks::<FRAME>().0 {
            denoiser.process(hop, &mut out);
            output.extend_from_slice(&out);
        }
        assert_eq!(&output[DELAY..], &ramp[..ramp.len() - DELAY]);
        assert!(output[..DELAY].iter().all(|&s| s == 0.0));
    }
}
