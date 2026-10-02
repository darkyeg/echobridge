//! A backend that plays scripted audio, for testing code that uses devices.
//!
//! Streams run on their own threads like real devices, but faster than real time: one
//! 10 ms block every `tick`. Device times advance exactly 10 ms per block, on one clock for
//! all streams. The loopback runs two blocks ahead of the microphone, as playback reaches
//! the loopback tap before its leak reaches the microphone.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{
    AudioBackend, CaptureBlock, CaptureCallback, CaptureInfo, Device, DeviceId, Direction, Error, Latency, RATE,
    RenderCallback, Source, Stream,
};

/// The device time of the first block, in seconds.
const START_TIME: f64 = 100.0;
/// Frames per tick for every stream.
const BLOCK: usize = RATE as usize / 100;

#[derive(Debug, Default)]
pub struct Script {
    /// Mono microphone audio; capture ends when it runs out.
    pub microphone: Vec<f32>,
    /// Interleaved stereo playback heard by the loopback.
    pub playback: Vec<f32>,
    /// Master volume reported with loopback blocks.
    pub playback_gain: f32,
    /// Real time per 10 ms block.
    pub tick: Duration,
    /// The output device stops with this failure after this many blocks.
    pub output_failure_after: Option<usize>,
}

#[derive(Debug)]
pub struct FakeBackend {
    script: Arc<Script>,
    /// Everything rendered to the cable, mono. Audio played to the headphones is discarded.
    pub rendered: Arc<Mutex<Vec<f32>>>,
}

pub const MICROPHONE: &str = "fake-microphone";
pub const HEADPHONES: &str = "fake-headphones";
pub const CABLE: &str = "fake-cable";

impl FakeBackend {
    pub fn new(script: Script) -> Self {
        Self { script: Arc::new(script), rendered: Arc::default() }
    }
}

impl AudioBackend for FakeBackend {
    fn devices(&self, direction: Direction) -> Result<Vec<Device>, Error> {
        let device = |id: &str, name: &str| Device { id: id.into(), name: name.into() };
        Ok(match direction {
            Direction::Input => vec![device(MICROPHONE, "Microphone (Fake)")],
            Direction::Output => vec![device(HEADPHONES, "Headphones (Fake)"), device(CABLE, "CABLE Input (Fake)")],
        })
    }

    fn default_device(&self, direction: Direction) -> Result<Option<DeviceId>, Error> {
        Ok(Some(match direction {
            Direction::Input => MICROPHONE.into(),
            Direction::Output => HEADPHONES.into(),
        }))
    }

    fn capture(
        &self,
        device: &DeviceId,
        source: Source,
        frames: usize,
        mut callback: CaptureCallback,
    ) -> Result<(Box<dyn Stream>, CaptureInfo), Error> {
        let expected = match source {
            Source::Microphone => MICROPHONE,
            Source::Loopback => HEADPHONES,
        };
        if device != expected || frames != BLOCK {
            return Err(Error::DeviceNotFound);
        }
        let script = self.script.clone();
        let (channels, lead) = match source {
            Source::Microphone => (1, 0),
            Source::Loopback => (2, 2),
        };
        let stream = FakeStream::spawn(script.tick, lead, move |block| {
            let (signal, gain) = match source {
                Source::Microphone => (&script.microphone, 1.0),
                Source::Loopback => (&script.playback, script.playback_gain),
            };
            let range = block * BLOCK * channels..(block + 1) * BLOCK * channels;
            let samples = signal.get(range).ok_or(None)?;
            let time = START_TIME + (block * BLOCK) as f64 / f64::from(RATE);
            callback(CaptureBlock { samples, channels, time, discontinuity: false, gain });
            Ok(())
        });
        Ok((Box::new(stream), CaptureInfo { channels, raw: source == Source::Microphone }))
    }

    fn render(
        &self,
        device: &DeviceId,
        _latency: Latency,
        mut callback: RenderCallback,
    ) -> Result<Box<dyn Stream>, Error> {
        if device != CABLE && device != HEADPHONES {
            return Err(Error::DeviceNotFound);
        }
        let cable = device == CABLE;
        let rendered = self.rendered.clone();
        let failure_after = self.script.output_failure_after.filter(|_| cable);
        let mut buffer = vec![0.0; BLOCK];
        let stream = FakeStream::spawn(self.script.tick, 0, move |block| {
            if failure_after.is_some_and(|after| block >= after) {
                return Err(Some("The output device was disconnected.".into()));
            }
            callback(&mut buffer);
            if cable {
                rendered.lock().unwrap().extend_from_slice(&buffer);
            }
            Ok(())
        });
        Ok(Box::new(stream))
    }
}

struct FakeStream {
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeStream {
    /// Call `step` with block numbers on a tick schedule, `lead` blocks early. `step`
    /// ends the stream with `Err(None)` or fails it with `Err(Some(message))`.
    fn spawn(
        tick: Duration,
        lead: usize,
        mut step: impl FnMut(usize) -> Result<(), Option<String>> + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread = {
            let (stop, failure) = (stop.clone(), failure.clone());
            std::thread::spawn(move || {
                let started = Instant::now();
                for block in 0.. {
                    let due = started + tick * (block as u32).saturating_sub(lead as u32);
                    while Instant::now() < due {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_micros(200));
                    }
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Err(error) = step(block) {
                        *failure.lock().unwrap() = error;
                        return;
                    }
                }
            })
        };
        Self { stop, failure, thread: Some(thread) }
    }
}

impl Stream for FakeStream {
    fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}
