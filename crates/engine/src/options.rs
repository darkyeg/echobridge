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
    /// Stock AEC3 suppression while only playback is present; a fast near-end speech
    /// detector switches to protected high frequencies while the user talks, so the voice
    /// keeps its clarity instead of being muffled whenever music plays.
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
    pub(crate) fn aec3_config(&self) -> Config {
        let tuning = match self.echo {
            EchoMode::CleanVoice | EchoMode::Strong => Tuning::default(),
            EchoMode::Adaptive => Tuning {
                nearend_detection: Some(NearendDetection {
                    enr_threshold: 10.0,
                    snr_threshold: 6.0,
                    trigger_blocks: 4,
                    hold_blocks: 60,
                }),
                nearend_high_frequencies: Some(Mask { enr_transparent: 30.0, enr_suppress: 31.0 }),
                ..Tuning::default()
            },
        };
        Config {
            stream_delay_ms: self.delay_ms.min(MAX_DELAY_MS),
            // AEC3 would suppress speech it cannot match to echo behind a full-band
            // canceller (measured: 16-24 dB of the voice when the leak was already gone).
            echo_cancellation: self.echo != EchoMode::CleanVoice,
            noise_suppression: (self.noise == NoiseRemoval::Standard).then_some(NoiseLevel::High),
            tuning,
            ..Config::default()
        }
    }
}
