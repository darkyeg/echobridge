//! EchoBridge's live microphone engine.
//!
//! ```text
//!  microphone ──► BlockClock ─┐
//!                             ├─► Pipeline ─► ElasticBuffer ─► output device (a virtual cable)
//!  loopback ──► ReferenceTimeline ┘   (linear canceller, AEC3 + noise suppression, AI denoiser)
//! ```
//!
//! [`Pipeline`] is the per-frame processing and needs no devices; [`Engine`] runs it live
//! on an [`AudioBackend`](echobridge_audio::AudioBackend). [`record_leak`] records the raw
//! microphone and playback for the leak test.

mod engine;
mod meters;
mod options;
mod pipeline;
mod recorder;

pub use engine::{Engine, EngineConfig, EngineError, Stats};
pub use meters::{Levels, Meters};
pub use options::{EchoMode, MAX_DELAY_MS, NoiseRemoval, Options};
pub use pipeline::{Pipeline, PipelineError, Stages};
pub use recorder::{RecordError, record_leak};
