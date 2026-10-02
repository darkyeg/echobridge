//! What the window says about the engine: one headline, one sentence of detail and a
//! tone for its color. Pure, so every case is tested without devices.

use crate::service::{Phase, Snapshot};

/// Playback quieter than this cannot show echo removal working.
const SILENT_PLAYBACK_DBFS: f32 = -65.0;
/// Below this share of playback reference, removal is unreliable.
const LOW_COVERAGE: f32 = 0.85;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Not running.
    Idle,
    /// Opening devices.
    Busy,
    /// Protecting.
    Good,
    /// Running without protection, or with a problem the user can fix.
    Warning,
    /// Stopped by an error.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub tone: Tone,
    pub headline: &'static str,
    pub detail: String,
}

/// `call_app_microphone` names what to choose in the call app, when an output is set.
pub fn describe(snapshot: &Snapshot, call_app_microphone: Option<&str>) -> Status {
    let status = |tone, headline, detail: &str| Status { tone, headline, detail: detail.into() };
    match &snapshot.phase {
        Phase::Off => status(Tone::Idle, "Off", "No microphone audio is sent. Press Start to protect your microphone."),
        Phase::Starting => status(Tone::Busy, "Starting", "Opening your microphone and playback devices…"),
        Phase::Failed(reason) => Status { tone: Tone::Error, headline: "Stopped", detail: sentence(reason) },
        Phase::Running => {
            let Some(stats) = &snapshot.stats else {
                return status(Tone::Busy, "Starting", "Waiting for the first microphone audio…");
            };
            if !stats.processing {
                return status(
                    Tone::Warning,
                    "Paused",
                    "Your original microphone passes through unchanged. Press Resume to remove the leak again.",
                );
            }
            let problem = if stats.ai_overloaded {
                Some("AI noise removal paused because the processor could not keep up. Choose Standard instead.")
            } else if stats.clipping {
                Some("Your microphone is too loud and clipping. Lower its level or turn off Microphone Boost.")
            } else if stats.frames > 100 && stats.reference_coverage < LOW_COVERAGE {
                Some("Playback audio is not arriving completely. Check that your apps play on the selected device.")
            } else {
                None
            };
            if let Some(problem) = problem {
                return status(Tone::Warning, "Protecting", problem);
            }
            let detail = match call_app_microphone {
                _ if stats.playback_dbfs < SILENT_PLAYBACK_DBFS => {
                    "Ready. Leaked playback will be removed as soon as something plays.".to_string()
                }
                Some(microphone) => format!("Leaked playback is removed. In your call app, choose {microphone}."),
                None => "Meters only: no output is selected, so nothing is sent to call apps.".to_string(),
            };
            Status { tone: Tone::Good, headline: "Protecting", detail }
        }
    }
}

/// Error texts end with a period in the window.
fn sentence(text: &str) -> String {
    let mut text = text.trim().to_string();
    if let Some(first) = text.get(..1) {
        text.replace_range(..1, &first.to_uppercase());
    }
    if !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    text
}

#[cfg(test)]
mod tests {
    use echobridge_engine::{EngineConfig, Options, Stats};

    use super::*;

    fn running(stats: Stats) -> Snapshot {
        let config = EngineConfig {
            microphone: "mic".into(),
            playback: "headphones".into(),
            output: Some("cable".into()),
            options: Options::default(),
            processing: stats.processing,
        };
        Snapshot { phase: Phase::Running, config: Some(config), stats: Some(stats) }
    }

    fn healthy() -> Stats {
        Stats { frames: 500, processing: true, playback_dbfs: -30.0, reference_coverage: 1.0, ..Stats::default() }
    }

    #[test]
    fn protecting_names_the_call_app_microphone() {
        let status = describe(&running(healthy()), Some("CABLE Output (VB-Audio Virtual Cable)"));
        assert_eq!(status.tone, Tone::Good);
        assert!(status.detail.contains("choose CABLE Output"), "{}", status.detail);
        let meters = describe(&running(healthy()), None);
        assert!(meters.detail.starts_with("Meters only"));
    }

    #[test]
    fn silence_and_pause_are_explained() {
        let quiet = describe(&running(Stats { playback_dbfs: -90.0, ..healthy() }), Some("CABLE Output"));
        assert!(quiet.detail.starts_with("Ready."));
        let paused = describe(&running(Stats { processing: false, ..healthy() }), None);
        assert_eq!((paused.tone, paused.headline), (Tone::Warning, "Paused"));
    }

    #[test]
    fn problems_are_reported_most_important_first() {
        let both = Stats { ai_overloaded: true, clipping: true, ..healthy() };
        assert!(describe(&running(both), None).detail.starts_with("AI noise removal"));
        let clipping = Stats { clipping: true, ..healthy() };
        assert!(describe(&running(clipping), None).detail.contains("clipping"));
        let gaps = Stats { reference_coverage: 0.5, ..healthy() };
        assert!(describe(&running(gaps), None).detail.contains("not arriving"));
        // Coverage is not judged before the reference had time to arrive.
        let early = Stats { frames: 10, reference_coverage: 0.0, ..healthy() };
        assert_eq!(describe(&running(early), None).tone, Tone::Good);
    }

    #[test]
    fn failures_read_as_sentences() {
        let failed = Snapshot {
            phase: Phase::Failed("the selected audio device is not connected".into()),
            ..Snapshot::default()
        };
        let status = describe(&failed, None);
        assert_eq!(status.tone, Tone::Error);
        assert_eq!(status.detail, "The selected audio device is not connected.");
    }
}
