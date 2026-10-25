//! What the user chooses: how the playback leak and background noise are removed.
//! Measurements behind each choice are in docs/AUDIO-DIAGNOSIS.md.

use echobridge_aec3::{Config, Mask, NearendDetection, NoiseLevel, Tuning};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EchoMode {
    /// Full-band linear cancellation only: no suppression gain ever touches the voice.
    /// Suits leaks that are linear across the spectrum, such as electrical crosstalk in a
    /// headset combo jack.
    #[serde(rename = "clean")]
    CleanVoice,
    /// Strong protection while the echo path is being learned or when echo risk rises; a
    /// second canceller with protected high frequencies reaches the call only after
    /// measurements show the playback leak has no high-frequency content, so the voice
    /// keeps its clarity. When the measurements are inconclusive it stays on Strong.
    #[default]
    Adaptive,
    /// Stock AEC3 suppression: the most removal, but it muffles speech above 4 kHz.
    Strong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoiseRemoval {
    /// The call app handles background noise.
    #[default]
    Off,
    /// WebRTC noise suppression at Chrome's level (about 18 dB) after echo removal.
    Standard,
    /// DeepFilterNet3 after echo removal: cleaner and gentler on the voice; adds 30 ms.
    Ai,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Options {
    pub echo: EchoMode,
    pub noise: NoiseRemoval,
    /// Known playback-to-microphone delay for AEC3, in ms; 0 finds it automatically.
    pub delay_ms: u32,
}

/// Longest echo delay a user may set, in ms.
pub const MAX_DELAY_MS: u32 = 250;

impl Options {
    /// The AEC3 stage for these options; it always runs, for its high-pass filter.
    /// Adaptive uses Strong's tuning here: it is the protected path.
    pub(crate) fn aec3_config(&self) -> Config {
        Config {
            stream_delay_ms: self.delay_ms.min(MAX_DELAY_MS),
            // AEC3 would suppress speech it cannot match to echo behind a full-band
            // canceller (measured: 16-24 dB of the voice when the leak was already gone).
            echo_cancellation: self.echo != EchoMode::CleanVoice,
            noise_suppression: (self.noise == NoiseRemoval::Standard).then_some(NoiseLevel::High),
            tuning: Tuning::default(),
            ..Config::default()
        }
    }

    /// Adaptive's second AEC3 stage, trained on the same audio as [`Self::aec3_config`]
    /// but with a fast near-end detector that keeps the voice's high frequencies.
    pub(crate) fn release_config(&self) -> Option<Config> {
        (self.echo == EchoMode::Adaptive).then(|| Config {
            tuning: Tuning {
                nearend_detection: Some(NearendDetection {
                    enr_threshold: 10.0,
                    snr_threshold: 6.0,
                    trigger_blocks: 4,
                    hold_blocks: 60,
                }),
                nearend_high_frequencies: Some(Mask { enr_transparent: 30.0, enr_suppress: 31.0 }),
                ..Tuning::default()
            },
            ..self.aec3_config()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adaptive() -> Options {
        Options { echo: EchoMode::Adaptive, noise: NoiseRemoval::Standard, delay_ms: 20 }
    }

    #[test]
    fn adaptive_protects_with_strong_tuning_first() {
        let strong = Options { echo: EchoMode::Strong, ..adaptive() };
        assert_eq!(adaptive().aec3_config(), strong.aec3_config());
    }

    #[test]
    fn only_adaptive_has_a_second_stage() {
        for echo in [EchoMode::CleanVoice, EchoMode::Strong] {
            assert_eq!(Options { echo, ..adaptive() }.release_config(), None);
        }
        assert!(adaptive().release_config().is_some());
    }

    #[test]
    fn the_second_stage_differs_only_in_its_near_end_tuning() {
        let release = adaptive().release_config().unwrap();
        let primary = adaptive().aec3_config();
        assert_eq!(Config { tuning: primary.tuning, ..release }, primary);
        assert_ne!(release.tuning, primary.tuning);
        assert_eq!(release.noise_suppression, Some(NoiseLevel::High));
    }
}
