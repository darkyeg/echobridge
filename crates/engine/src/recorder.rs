//! Recording the raw microphone and playback together, for the leak test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echobridge_audio::{AudioBackend, CaptureBlock, DeviceId, Source};
use echobridge_dsp::FRAME;
use echobridge_dsp::leak::{AlignedRecording, Packet, align};

/// Loopback blocks can trail the last microphone block slightly.
const TAIL: Duration = Duration::from_millis(100);

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Audio(#[from] echobridge_audio::Error),
    #[error("The microphone delivered no audio. Check that it is connected.")]
    NoMicrophoneAudio,
    #[error("Recording was cancelled.")]
    Cancelled,
}

/// Record `duration` of the microphone and the playback of `playback`, in memory only,
/// aligned on one sample grid. The microphone is read as the engine reads it (without
/// system voice effects), so the leak measured is the one echo removal sees. This can run
/// beside a running engine: devices in shared mode allow several readers.
///
/// The playback is scaled by its master volume, so the measured coupling does not depend
/// on the volume setting.
pub fn record_leak(
    backend: &dyn AudioBackend,
    microphone: &DeviceId,
    playback: &DeviceId,
    duration: Duration,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f32),
) -> Result<AlignedRecording, RecordError> {
    let collector = |packets: Arc<Mutex<Vec<Packet>>>| {
        Box::new(move |block: CaptureBlock<'_>| {
            let mut samples = Vec::with_capacity(block.frames());
            let channels = block.channels.clamp(1, 2);
            for frame in block.samples.chunks_exact(block.channels) {
                samples.push(frame[..channels].iter().sum::<f32>() / channels as f32 * block.gain);
            }
            packets.lock().unwrap().push(Packet { timestamp: block.time, samples });
        })
    };
    let reference = Arc::new(Mutex::new(Vec::new()));
    let near = Arc::new(Mutex::new(Vec::new()));
    {
        let _loopback = backend.capture(playback, Source::Loopback, FRAME, collector(reference.clone()))?;
        let _microphone = backend.capture(microphone, Source::Microphone, FRAME, collector(near.clone()))?;
        let started = Instant::now();
        while started.elapsed() < duration {
            if cancel.load(Ordering::Relaxed) {
                return Err(RecordError::Cancelled);
            }
            progress((started.elapsed().as_secs_f32() / duration.as_secs_f32()).min(1.0));
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(TAIL);
    }
    progress(1.0);
    let near = std::mem::take(&mut *near.lock().unwrap());
    if near.is_empty() {
        return Err(RecordError::NoMicrophoneAudio);
    }
    let reference = std::mem::take(&mut *reference.lock().unwrap());
    Ok(align(&near, &reference))
}
