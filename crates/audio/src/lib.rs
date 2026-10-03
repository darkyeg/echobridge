//! Audio device access for EchoBridge.
//!
//! Everything above this crate talks to an [`AudioBackend`]: list devices, capture a
//! microphone or the playback of an output device (loopback), and render to an output
//! device. Each operating system implements the trait once (WASAPI on Windows; PipeWire is
//! the natural Linux backend). A backend for EchoBridge's own virtual microphone would
//! appear as one more output device.
//!
//! All audio is 48 kHz 32-bit float. Capture is delivered in fixed-size blocks stamped
//! with the device time of their first sample, on a clock that the microphone and the
//! loopback share, so the engine can align them to a fraction of a sample.

use std::sync::Arc;

mod blocks;
#[cfg(feature = "fake")]
pub mod fake;
mod priority;
#[cfg(any(windows, test))]
mod timestamps;
#[cfg(windows)]
mod wasapi;

pub use blocks::BlockAssembler;
pub use priority::AudioThreadPriority;

/// The sample rate of every stream.
pub const RATE: u32 = 48_000;

/// A stable device identifier, valid until the device is removed from the system.
pub type DeviceId = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub id: DeviceId,
    /// The name the system shows, such as "Headphones (Realtek Audio)".
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Microphones and other recording devices.
    Input,
    /// Headphones, speakers, and virtual cables.
    Output,
}

/// What a capture stream records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An input device, without system voice effects where the device allows it: they
    /// denoise and gate the microphone before echo cancellation and distort the leak.
    Microphone,
    /// What an output device plays, as the echo reference.
    Loopback,
}

/// One block of captured audio.
#[derive(Debug, Clone, Copy)]
pub struct CaptureBlock<'a> {
    /// Interleaved samples, `frames * channels`.
    pub samples: &'a [f32],
    pub channels: usize,
    /// Device time of the first sample, in seconds.
    pub time: f64,
    /// Samples were lost before this block.
    pub discontinuity: bool,
    /// Loopback only: the volume the device applies after the loopback tap (0 when muted).
    /// A wired leak follows it, so the reference must be scaled by it.
    pub gain: f32,
}

impl CaptureBlock<'_> {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }
}

/// Called on the capture thread with each block. It must return quickly.
pub type CaptureCallback = Box<dyn FnMut(CaptureBlock<'_>) + Send>;
/// Called on the render thread to fill mono samples for the device.
pub type RenderCallback = Box<dyn FnMut(&mut [f32]) + Send>;

/// Facts about an opened capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureInfo {
    pub channels: usize,
    /// The microphone is captured without system voice effects.
    pub raw: bool,
}

/// A running stream; dropping it stops the stream and waits for its thread.
pub trait Stream: Send {
    /// Why the stream stopped, if it did (a device was unplugged, for example).
    fn failure(&self) -> Option<String>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the selected audio device is not connected")]
    DeviceNotFound,
    #[error("{0}")]
    System(String),
    #[error("audio devices are not supported on this system yet")]
    Unsupported,
}

/// How much an output stream may buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Latency {
    /// The smallest buffer the device allows: for the clean microphone, where every
    /// millisecond reaches the call.
    Low,
    /// The system's usual buffer: for streams nobody hears, which should not change how
    /// the device runs for other apps.
    Normal,
}

pub trait AudioBackend: Send + Sync {
    fn devices(&self, direction: Direction) -> Result<Vec<Device>, Error>;
    /// The system's default device, if there is one.
    fn default_device(&self, direction: Direction) -> Result<Option<DeviceId>, Error>;
    /// Capture `source` from `device`, delivering blocks of `frames` frames.
    fn capture(
        &self,
        device: &DeviceId,
        source: Source,
        frames: usize,
        callback: CaptureCallback,
    ) -> Result<(Box<dyn Stream>, CaptureInfo), Error>;
    /// Play mono audio from `callback` on `device`, on all of its channels.
    fn render(&self, device: &DeviceId, latency: Latency, callback: RenderCallback) -> Result<Box<dyn Stream>, Error>;
}

/// The backend for this operating system.
pub fn system_backend() -> Result<Arc<dyn AudioBackend>, Error> {
    #[cfg(windows)]
    {
        Ok(Arc::new(wasapi::Wasapi))
    }
    #[cfg(not(windows))]
    {
        Err(Error::Unsupported)
    }
}
