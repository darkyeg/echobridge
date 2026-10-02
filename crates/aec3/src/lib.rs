//! WebRTC AEC3 echo cancellation, noise suppression, and the high-pass filter, as Chrome
//! applies them to a microphone, behind a safe Rust API.
//!
//! The C++ lives in `cpp/shim.cpp`; this module owns its one pointer.

use std::ffi::{CStr, c_char, c_int};
use std::ptr::NonNull;

mod ffi {
    use std::ffi::{c_char, c_int};

    #[repr(C)]
    pub struct EbAec {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct EbNearendDetection {
        pub enr_threshold: f32,
        pub snr_threshold: f32,
        pub trigger_blocks: c_int,
        pub hold_blocks: c_int,
    }

    #[repr(C)]
    pub struct EbMask {
        pub enr_transparent: f32,
        pub enr_suppress: f32,
    }

    #[repr(C)]
    pub struct EbAecConfig {
        pub sample_rate: c_int,
        pub channels: c_int,
        pub stream_delay_ms: c_int,
        pub echo_cancellation: c_int,
        pub allow_transparent_mode: c_int,
        pub noise_suppression: c_int,
        pub nearend_detection: *const EbNearendDetection,
        pub nearend_lf: *const EbMask,
        pub nearend_hf: *const EbMask,
        pub normal_follows_nearend: c_int,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct EbAecMetrics {
        pub echo_return_loss_db: f64,
        pub echo_return_loss_enhancement_db: f64,
        pub delay_ms: c_int,
    }

    unsafe extern "C" {
        pub fn eb_aec_create(config: *const EbAecConfig, error: *mut c_char, error_size: usize) -> *mut EbAec;
        pub fn eb_aec_process(aec: *mut EbAec, near: *const f32, far: *const f32, out: *mut f32) -> c_int;
        pub fn eb_aec_reset(aec: *mut EbAec);
        pub fn eb_aec_metrics(aec: *const EbAec, metrics: *mut EbAecMetrics);
        pub fn eb_aec_free(aec: *mut EbAec);
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("{0}")]
    Config(String),
    #[error("provide one 10 ms frame of interleaved audio ({0} values) for each signal")]
    FrameSize(usize),
    #[error("audio samples must be finite")]
    NonFinite,
}

/// WebRTC noise suppression strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseLevel {
    /// About 6 dB.
    Low,
    /// About 12 dB.
    Moderate,
    /// About 18 dB, as Chrome uses.
    High,
    /// About 21 dB.
    VeryHigh,
}

/// Dominant near-end (user speaking) detection thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NearendDetection {
    pub enr_threshold: f32,
    pub snr_threshold: f32,
    pub trigger_blocks: u32,
    pub hold_blocks: u32,
}

/// Echo-to-near-end ratios between which a suppressor mask goes from transparent to full.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mask {
    pub enr_transparent: f32,
    pub enr_suppress: f32,
}

/// Overrides of AEC3's suppressor; `None` keeps its default.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Tuning {
    pub nearend_detection: Option<NearendDetection>,
    pub nearend_low_frequencies: Option<Mask>,
    pub nearend_high_frequencies: Option<Mask>,
    /// Apply the near-end masks while the user is silent too.
    pub normal_follows_nearend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    /// 16000, 32000, or 48000 Hz.
    pub sample_rate: u32,
    /// 1 or 2, for both the microphone and the reference.
    pub channels: usize,
    /// Known playback-to-microphone delay; 0 lets AEC3 find it.
    pub stream_delay_ms: u32,
    /// `false` runs only the high-pass filter and noise suppression, for callers that
    /// remove the leak themselves.
    pub echo_cancellation: bool,
    /// AEC3 assumes an echo-free headset after six seconds without a converged filter; it
    /// recognizes convergence only above about -56 dBFS, so a quiet steady leak would turn
    /// echo removal off. Off by default.
    pub allow_transparent_mode: bool,
    pub noise_suppression: Option<NoiseLevel>,
    pub tuning: Tuning,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            stream_delay_ms: 0,
            echo_cancellation: true,
            allow_transparent_mode: false,
            noise_suppression: None,
            tuning: Tuning::default(),
        }
    }
}

/// AEC3 statistics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub echo_return_loss_db: f64,
    pub echo_return_loss_enhancement_db: f64,
    pub delay_ms: i32,
}

/// One microphone's echo canceller and noise suppressor.
#[derive(Debug)]
pub struct EchoProcessor {
    raw: NonNull<ffi::EbAec>,
    frame_values: usize,
}

// SAFETY: the processor owns its C++ state exclusively and does not use thread-local
// storage, so it may move between threads; `&mut self` serializes all use.
unsafe impl Send for EchoProcessor {}

impl EchoProcessor {
    pub fn new(config: &Config) -> Result<Self, Error> {
        let to_int = |value: u32| c_int::try_from(value).unwrap_or(c_int::MAX);
        let detection = config.tuning.nearend_detection.map(|d| ffi::EbNearendDetection {
            enr_threshold: d.enr_threshold,
            snr_threshold: d.snr_threshold,
            trigger_blocks: to_int(d.trigger_blocks),
            hold_blocks: to_int(d.hold_blocks),
        });
        let mask = |m: Option<Mask>| {
            m.map(|m| ffi::EbMask { enr_transparent: m.enr_transparent, enr_suppress: m.enr_suppress })
        };
        let (lf, hf) = (mask(config.tuning.nearend_low_frequencies), mask(config.tuning.nearend_high_frequencies));
        let raw_config = ffi::EbAecConfig {
            sample_rate: to_int(config.sample_rate),
            channels: c_int::try_from(config.channels).unwrap_or(0),
            stream_delay_ms: to_int(config.stream_delay_ms),
            echo_cancellation: config.echo_cancellation.into(),
            allow_transparent_mode: config.allow_transparent_mode.into(),
            noise_suppression: config.noise_suppression.map_or(-1, |level| level as c_int),
            nearend_detection: pointer(&detection),
            nearend_lf: pointer(&lf),
            nearend_hf: pointer(&hf),
            normal_follows_nearend: config.tuning.normal_follows_nearend.into(),
        };
        let mut error = [0 as c_char; 256];
        // SAFETY: `raw_config` and the overrides it points to outlive the call, which
        // copies them; `error` is writable for its full length.
        let raw = unsafe { ffi::eb_aec_create(&raw_config, error.as_mut_ptr(), error.len()) };
        match NonNull::new(raw) {
            Some(raw) => Ok(Self { raw, frame_values: (config.sample_rate / 100) as usize * config.channels }),
            None => {
                // SAFETY: the shim wrote a NUL-terminated message within the buffer.
                let message = unsafe { CStr::from_ptr(error.as_ptr()) };
                Err(Error::Config(message.to_string_lossy().into_owned()))
            }
        }
    }

    /// Interleaved values in one 10 ms frame.
    pub fn frame_values(&self) -> usize {
        self.frame_values
    }

    /// Process one 10 ms frame of interleaved microphone `near` and playback `far` audio.
    pub fn process(&mut self, near: &[f32], far: &[f32], out: &mut [f32]) -> Result<(), Error> {
        let n = self.frame_values;
        if near.len() != n || far.len() != n || out.len() != n {
            return Err(Error::FrameSize(n));
        }
        // SAFETY: all three buffers hold exactly one frame, as the shim reads and writes.
        match unsafe { ffi::eb_aec_process(self.raw.as_ptr(), near.as_ptr(), far.as_ptr(), out.as_mut_ptr()) } {
            0 => Ok(()),
            _ => Err(Error::NonFinite),
        }
    }

    /// Forget the echo path and noise estimates, as after a gap in the audio.
    pub fn reset(&mut self) {
        // SAFETY: `raw` is valid until drop.
        unsafe { ffi::eb_aec_reset(self.raw.as_ptr()) }
    }

    pub fn metrics(&self) -> Metrics {
        let mut metrics = ffi::EbAecMetrics::default();
        // SAFETY: `raw` is valid until drop and `metrics` is writable.
        unsafe { ffi::eb_aec_metrics(self.raw.as_ptr(), &mut metrics) };
        Metrics {
            echo_return_loss_db: metrics.echo_return_loss_db,
            echo_return_loss_enhancement_db: metrics.echo_return_loss_enhancement_db,
            delay_ms: metrics.delay_ms,
        }
    }
}

fn pointer<T>(value: &Option<T>) -> *const T {
    value.as_ref().map_or(std::ptr::null(), |v| v as *const T)
}

impl Drop for EchoProcessor {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `eb_aec_create` and is freed exactly once.
        unsafe { ffi::eb_aec_free(self.raw.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_configuration_with_a_message() {
        let error = EchoProcessor::new(&Config { sample_rate: 44_100, ..Config::default() }).unwrap_err();
        assert_eq!(error, Error::Config("Sample rate must be 16000, 32000, or 48000 Hz".into()));
    }

    #[test]
    fn rejects_wrong_frame_sizes_and_non_finite_samples() {
        let mut processor = EchoProcessor::new(&Config::default()).unwrap();
        let mut out = vec![0.0; 960];
        assert_eq!(processor.process(&[0.0; 10], &[0.0; 960], &mut out), Err(Error::FrameSize(960)));
        let mut near = vec![0.0; 960];
        near[3] = f32::NAN;
        assert_eq!(processor.process(&near, &[0.0; 960], &mut out), Err(Error::NonFinite));
    }

    /// Playback with a speech-like rhythm and most energy below 8 kHz (where AEC3
    /// subtracts rather than only suppresses), leaking at -20 dB with a 5 ms delay. Steady
    /// noise would not do: AEC3 takes a stationary signal for background noise.
    #[test]
    fn removes_a_delayed_echo() {
        let mut processor = EchoProcessor::new(&Config::default()).unwrap();
        let mut state = 12345u32;
        let mut noise = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut smoothed = 0.0;
        let playback: Vec<f32> = (0..48_000 * 8)
            .map(|t| {
                smoothed = 0.8 * smoothed + 0.2 * noise();
                let syllable = (std::f32::consts::TAU * 3.0 * t as f32 / 48_000.0).sin().max(0.0);
                smoothed * syllable
            })
            .collect();
        let delay = 240;
        let (mut before, mut after) = (0.0f64, 0.0f64);
        let mut out = vec![0.0; 960];
        for frame in 0..800 {
            let mut near = vec![0.0; 960];
            let mut far = vec![0.0; 960];
            for i in 0..480 {
                let t = frame * 480 + i;
                far[2 * i] = playback[t];
                far[2 * i + 1] = playback[t];
                let echo = if t >= delay { 0.1 * playback[t - delay] } else { 0.0 };
                near[2 * i] = echo;
                near[2 * i + 1] = echo;
            }
            processor.process(&near, &far, &mut out).unwrap();
            if frame >= 400 {
                before += near.iter().map(|&s| f64::from(s * s)).sum::<f64>();
                after += out.iter().map(|&s| f64::from(s * s)).sum::<f64>();
            }
        }
        let removed = 10.0 * (before / after.max(1e-20)).log10();
        assert!(removed > 20.0, "removed {removed:.1} dB");
    }
}
