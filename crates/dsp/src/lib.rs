//! Platform-independent signal processing for EchoBridge.
//!
//! Nothing here touches an audio device or a thread: every type is plain data that the
//! engine drives, so all of it can be tested with synthetic or recorded audio.
//!
//! Audio is 48 kHz float. Multichannel audio is interleaved; the engine works on mono
//! microphone audio and a stereo playback reference.

pub mod clock;
pub mod delay;
pub mod elastic;
pub mod fft;
pub mod leak;
pub mod level;
pub mod limiter;
pub mod linear;
pub mod timeline;

/// Processing sample rate in Hz.
pub const RATE: u32 = 48_000;
/// Samples per processing frame: 10 ms, the unit of WebRTC and DeepFilterNet.
pub const FRAME: usize = 480;

/// One stereo sample.
pub type Stereo = [f32; 2];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DspError {
    #[error("expected whole blocks of {block} samples with matching reference length")]
    BlockMismatch { block: usize },
    #[error("record at least {seconds} second(s) of microphone and playback audio")]
    TooShort { seconds: u32 },
}

/// Convert interleaved audio with any channel count to stereo frames.
///
/// Mono is duplicated; channels after the first two are dropped, as a playback reference
/// keeps its front pair.
pub fn to_stereo(interleaved: &[f32], channels: usize, out: &mut Vec<Stereo>) {
    out.clear();
    match channels {
        0 => {}
        1 => out.extend(interleaved.iter().map(|&s| [s, s])),
        _ => out.extend(interleaved.chunks_exact(channels).map(|f| [f[0], f[1]])),
    }
}

/// Mix interleaved audio with any channel count down to mono.
pub fn to_mono(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    if channels == 0 {
        return;
    }
    let scale = 1.0 / channels as f32;
    out.extend(interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() * scale));
}
