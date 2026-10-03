//! The live engine: device streams, the processing thread, and its statistics.
//!
//! Threads:
//! - the loopback stream appends playback to a [`ReferenceTimeline`];
//! - the microphone stream queues 10 ms blocks;
//! - the processing thread (audio priority) places each block on the device clock, waits
//!   briefly for the playback that played during it, runs the [`Pipeline`], and feeds an
//!   [`ElasticBuffer`];
//! - the output stream drains that buffer into the output device.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use echobridge_audio::{AudioBackend, AudioThreadPriority, CaptureBlock, DeviceId, Latency, Source, Stream};
use echobridge_dsp::clock::BlockClock;
use echobridge_dsp::delay::Alignment;
use echobridge_dsp::elastic::ElasticBuffer;
use echobridge_dsp::level::{FLOOR_DBFS, clipping, dbfs};
use echobridge_dsp::timeline::ReferenceTimeline;
use echobridge_dsp::{FRAME, RATE, Stereo};
use serde::Serialize;

use crate::meters::{Levels, Meters};
use crate::options::Options;
use crate::pipeline::{Pipeline, PipelineError, Stages};

/// Microphone blocks waiting for processing; more means the processor fell behind.
const QUEUE_BLOCKS: usize = 15;
/// How long a microphone block may wait for its playback. The deadline counts from the
/// block's arrival, so queued blocks cannot renew it.
const REFERENCE_WAIT: Duration = Duration::from_millis(20);
/// Processing a frame and handing it over must fit in what is left of the output's audio
/// when the wait for the reference ends, in seconds.
const OUTPUT_MARGIN: f64 = 0.003;
/// Stream health is checked this often, and whenever the microphone goes quiet.
const HEALTH_CHECK_FRAMES: u64 = 50;
const IDLE_WAIT: Duration = Duration::from_millis(250);
/// Meters fall by this much per 10 ms frame (20 dB per second) and rise instantly.
const METER_RELEASE_DB: f32 = 0.2;
/// The clipping warning stays on for a second after the last clipped frame.
const CLIP_HOLD_FRAMES: u64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub microphone: DeviceId,
    /// The output device whose playback leaks into the microphone.
    pub playback: DeviceId,
    /// Where the clean microphone goes, usually a virtual cable; `None` only meters.
    pub output: Option<DeviceId>,
    pub options: Options,
    /// `false` passes the microphone through unprocessed.
    pub processing: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("The clean microphone cannot be sent to the playback device it listens to.")]
    OutputIsPlayback,
    #[error(transparent)]
    Audio(#[from] echobridge_audio::Error),
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    #[error("The audio engine stopped unexpectedly.")]
    Stopped,
}

/// A snapshot of the engine, for display and diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    /// Frames processed since start.
    pub frames: u64,
    /// The leak is being removed; `false` passes the microphone through.
    pub processing: bool,
    pub options: Options,
    /// Meter levels in dBFS: peaks with a 20 dB/s release.
    pub microphone_dbfs: f32,
    pub playback_dbfs: f32,
    pub output_dbfs: f32,
    /// Share of the last frame with playback reference, from 0 to 1.
    pub reference_coverage: f32,
    /// The microphone reached full scale within the last second.
    pub clipping: bool,
    /// Microphone frames with samples near full scale, including those between UI polls.
    pub clipped_frames: u64,
    /// The microphone is captured without system voice effects.
    pub raw_microphone: bool,
    /// The processing thread runs at audio priority.
    pub audio_priority: bool,
    /// AI noise removal stopped because the processor could not keep up.
    pub ai_overloaded: bool,
    pub dropped_blocks: u64,
    pub output_underflows: u64,
    pub output_trims: u64,
    pub output_padding_events: u64,
    /// Processed audio waiting for the output device, in ms: part of the delay.
    pub output_buffer_ms: f32,
    /// How much later than each microphone frame its playback reference is read, in ms,
    /// to keep the leak where the canceller can model it. Positive values add delay.
    pub reference_shift_ms: f32,
    pub incomplete_reference_frames: u64,
    /// Full retrains after resume, new options, or a change in reference/clock mapping.
    pub canceller_retrains: u64,
    /// Interrupted stream histories cleared while retaining the accepted Clean voice path.
    pub stream_resets: u64,
    /// Playback timeline restarts, excluding the first capture block.
    pub reference_discontinuities: u64,
    pub processing_ms: f32,
    pub reference_wait_ms: f32,
}

enum Command {
    Apply(Box<Stages>),
}

struct Shared {
    stop: AtomicBool,
    processing: AtomicBool,
    stats: Mutex<Stats>,
    meters: Arc<Meters>,
    failure: Mutex<Option<String>>,
}

/// A running engine; dropping it stops all streams.
pub struct Engine {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    options: Options,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").field("options", &self.options).finish_non_exhaustive()
    }
}

impl Engine {
    /// Open the devices and start processing. Returns once audio flows or failed to.
    pub fn start(backend: Arc<dyn AudioBackend>, config: EngineConfig) -> Result<Self, EngineError> {
        if config.output.as_ref() == Some(&config.playback) {
            return Err(EngineError::OutputIsPlayback);
        }
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            processing: AtomicBool::new(config.processing),
            stats: Mutex::new(Stats { options: config.options, processing: config.processing, ..Stats::default() }),
            meters: Arc::default(),
            failure: Mutex::new(None),
        });
        let (commands, receiver) = channel();
        let (ready, started) = sync_channel(1);
        let options = config.options;
        let worker = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("EchoBridge processing".into())
                .spawn(move || Worker::run(backend, config, shared, receiver, ready))
                .map_err(|_| EngineError::Stopped)?
        };
        let mut engine = Self { shared, commands, options, worker: Some(worker) };
        match started.recv() {
            Ok(Ok(())) => Ok(engine),
            Ok(Err(error)) => {
                engine.join();
                Err(error)
            }
            Err(_) => {
                engine.join();
                Err(EngineError::Stopped)
            }
        }
    }

    pub fn options(&self) -> Options {
        self.options
    }

    /// Change options without reopening any device. Slow preparation (loading the AI
    /// model) happens on the calling thread, not the audio thread.
    pub fn set_options(&mut self, options: Options) -> Result<(), EngineError> {
        if options == self.options {
            return Ok(());
        }
        let stages = Stages::prepare(options, Some(&self.options))?;
        self.commands.send(Command::Apply(Box::new(stages))).map_err(|_| EngineError::Stopped)?;
        self.options = options;
        Ok(())
    }

    /// `false` passes the microphone through unprocessed; `true` resumes processing.
    pub fn set_processing(&self, processing: bool) {
        self.shared.processing.store(processing, Ordering::Relaxed);
    }

    pub fn stats(&self) -> Stats {
        self.shared.stats.lock().unwrap().clone()
    }

    /// The live meter levels, for display at any rate. The handle stays valid after the
    /// engine stops; the levels then stay where they were.
    pub fn meters(&self) -> Arc<Meters> {
        self.shared.meters.clone()
    }

    /// Why the engine stopped, if it did: a device was unplugged, for example.
    pub fn failure(&self) -> Option<String> {
        self.shared.failure.lock().unwrap().clone()
    }

    /// Stop all streams and finish the current frame, keeping the final statistics.
    pub fn stop(&mut self) {
        self.join();
    }

    fn join(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().ok();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.join();
    }
}

/// One microphone block, mixed to mono.
struct MicBlock {
    samples: [f32; FRAME],
    time: f64,
    discontinuity: bool,
    arrived: Instant,
    skipped: usize,
}

/// Queue losses have a known duration: keep it and any device discontinuity until the
/// next block is accepted, without making capture wait for the processor.
struct MicQueue {
    sender: SyncSender<MicBlock>,
    dropped: Arc<AtomicU64>,
    skipped: usize,
    discontinuity: bool,
}

impl MicQueue {
    fn send(&mut self, mut block: MicBlock) {
        block.skipped = self.skipped;
        block.discontinuity |= self.discontinuity;
        match self.sender.try_send(block) {
            Ok(()) => {
                self.skipped = 0;
                self.discontinuity = false;
            }
            Err(TrySendError::Full(block)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.skipped += 1;
                self.discontinuity |= block.discontinuity;
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// Playback on the microphone's clock, shared with the loopback thread.
struct Reference {
    timeline: Mutex<ReferenceTimeline>,
    arrived: Condvar,
}

struct Worker {
    backend: Arc<dyn AudioBackend>,
    config: EngineConfig,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    pipeline: Pipeline,
    reference: Arc<Reference>,
    output: Arc<Mutex<ElasticBuffer>>,
    dropped: Arc<AtomicU64>,
    clock: BlockClock,
    alignment: Alignment,
    was_processing: bool,
    reference_generation: u64,
    reference_complete: bool,
    last_clip: Option<u64>,
    far: [Stereo; FRAME],
    clean: [f32; FRAME],
}

impl Worker {
    fn run(
        backend: Arc<dyn AudioBackend>,
        config: EngineConfig,
        shared: Arc<Shared>,
        commands: Receiver<Command>,
        ready: SyncSender<Result<(), EngineError>>,
    ) {
        let priority = AudioThreadPriority::raise();
        let mut ready = Some(ready);
        let result = Pipeline::new(config.options).map_err(EngineError::from).and_then(|pipeline| {
            let mut worker = Self {
                backend,
                config,
                shared: shared.clone(),
                commands,
                pipeline,
                reference: Arc::new(Reference {
                    timeline: Mutex::new(ReferenceTimeline::new(RATE)),
                    arrived: Condvar::new(),
                }),
                output: Arc::new(Mutex::new(ElasticBuffer::new(RATE))),
                dropped: Arc::default(),
                clock: BlockClock::live(RATE),
                alignment: Alignment::default(),
                was_processing: false,
                reference_generation: 0,
                reference_complete: false,
                last_clip: None,
                far: [[0.0; 2]; FRAME],
                clean: [0.0; FRAME],
            };
            worker.shared.stats.lock().unwrap().audio_priority = priority.granted();
            let result = worker.stream(&mut ready);
            // All streams have joined: include their last callbacks in shutdown health.
            let output = worker.output.lock().unwrap();
            let mut stats = worker.shared.stats.lock().unwrap();
            stats.output_underflows = output.underflows;
            stats.output_trims = output.trims;
            stats.output_padding_events = output.padding_events;
            stats.dropped_blocks = worker.dropped.load(Ordering::Relaxed);
            stats.reference_discontinuities = worker.reference.timeline.lock().unwrap().generation();
            result
        });
        if let Err(error) = result {
            log::warn!("audio engine stopped: {error}");
            match ready.take() {
                Some(ready) => {
                    ready.send(Err(error)).ok();
                }
                None => *shared.failure.lock().unwrap() = Some(error.to_string()),
            }
        }
    }

    /// Open the streams and process until stopped. Streams close in reverse order on return.
    fn stream(&mut self, ready: &mut Option<SyncSender<Result<(), EngineError>>>) -> Result<(), EngineError> {
        let reference = self.reference.clone();
        let mut stereo = Vec::with_capacity(FRAME);
        let (loopback, _) = self.backend.capture(
            &self.config.playback,
            Source::Loopback,
            FRAME,
            Box::new(move |block: CaptureBlock<'_>| {
                echobridge_dsp::to_stereo(block.samples, block.channels, &mut stereo);
                // Windows applies the master volume after the loopback tap, but a wired leak
                // follows it; scaling keeps a volume change from looking like a new leak.
                if block.gain != 1.0 {
                    stereo.iter_mut().flatten().for_each(|s| *s *= block.gain);
                }
                {
                    let mut timeline = reference.timeline.lock().unwrap();
                    if block.discontinuity {
                        timeline.clear();
                    }
                    timeline.append(&stereo, block.time);
                }
                reference.arrived.notify_all();
            }),
        )?;
        // Windows loopback delivers nothing while its device is silent, which would read as
        // a missing reference and make every frame wait for it. Playing silence keeps the
        // loopback flowing, as OBS does; it changes nothing audible.
        let keep_alive =
            self.backend.render(&self.config.playback, Latency::Normal, Box::new(|out: &mut [f32]| out.fill(0.0)))?;
        let (sender, blocks) = sync_channel(QUEUE_BLOCKS);
        let mut queue = MicQueue { sender, dropped: self.dropped.clone(), skipped: 0, discontinuity: false };
        let mut mono = Vec::with_capacity(FRAME);
        let (microphone, info) = self.backend.capture(
            &self.config.microphone,
            Source::Microphone,
            FRAME,
            Box::new(move |block: CaptureBlock<'_>| {
                echobridge_dsp::to_mono(block.samples, block.channels, &mut mono);
                let mut samples = [0.0; FRAME];
                samples.copy_from_slice(&mono);
                queue.send(MicBlock {
                    samples,
                    time: block.time,
                    discontinuity: block.discontinuity,
                    arrived: Instant::now(),
                    skipped: 0,
                });
            }),
        )?;
        self.shared.stats.lock().unwrap().raw_microphone = info.raw;
        if let Some(ready) = ready.take() {
            ready.send(Ok(())).ok();
        }
        let mut render: Option<Box<dyn Stream>> = None;
        let mut frames = 0u64;
        while !self.shared.stop.load(Ordering::Relaxed) {
            while let Ok(Command::Apply(stages)) = self.commands.try_recv() {
                self.pipeline.apply(*stages);
                self.was_processing = false; // start the new stages from a clean state
            }
            let block = match blocks.recv_timeout(IDLE_WAIT) {
                Ok(block) => block,
                Err(RecvTimeoutError::Timeout) => {
                    check(&[Some(&*loopback), Some(&*keep_alive), Some(&*microphone), render.as_deref()])?;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    check(&[Some(&*microphone)])?;
                    return Err(EngineError::Stopped);
                }
            };
            self.process(&block, frames)?;
            if let Some(output) = &self.config.output {
                // Opened once audio flows, so the device does not start on silence.
                if render.is_none() {
                    let buffer = self.output.clone();
                    render = Some(self.backend.render(
                        output,
                        Latency::Low,
                        Box::new(move |out: &mut [f32]| buffer.lock().unwrap().pull(out)),
                    )?);
                }
            }
            frames += 1;
            if frames.is_multiple_of(HEALTH_CHECK_FRAMES) {
                check(&[Some(&*loopback), Some(&*keep_alive), Some(&*microphone), render.as_deref()])?;
            }
        }
        Ok(())
    }

    fn process(&mut self, block: &MicBlock, frame: u64) -> Result<(), EngineError> {
        let started = Instant::now();
        let expected = self.clock.next();
        if block.discontinuity {
            self.clock.reset();
        } else if block.skipped > 0 {
            self.clock.skip(block.skipped * FRAME);
        }
        let placed = self.clock.place(block.time, FRAME);
        let phase_changed = placed.restarted
            && expected.is_some_and(|next| {
                let period = FRAME as f64 / f64::from(RATE);
                let gap = placed.start - next;
                (gap - (gap / period).round() * period).abs() > 0.5 / f64::from(RATE)
            });
        let processing = self.shared.processing.load(Ordering::Relaxed);
        // A reference read later than the frame also arrives that much later. Never wait past
        // the audio the output still holds: a late reference lets a little leak through for a
        // moment, while an empty output is a gap the call app hears.
        let start = placed.start + self.alignment.shift();
        let mut reserved = if processing { self.reserve_for_reference(start, block) } else { 0 };
        let deadline = processing.then(|| self.reference_deadline(block));
        let (mut coverage, mut generation) = self.wait_for_reference(placed.start + self.alignment.shift(), deadline);
        let mut reference_wait = started.elapsed();
        let interrupted = placed.restarted || block.skipped > 0 || generation != self.reference_generation;
        if interrupted || !self.was_processing || coverage < 0.99 {
            self.alignment.clear_history();
        }
        let (mut retrained, mut stream_reset) = (false, false);
        if processing {
            let realigned = coverage > 0.99 && self.alignment.update(&block.samples, &self.far);
            if realigned {
                let shift = self.alignment.shift();
                log::info!("leak moved; reading the reference {:.1} ms after the microphone", shift * 1e3);
                // The alignment measurement used the old read point. Process this frame
                // with the new reference too, so a retrain never starts on the wrong path.
                let waited = Instant::now();
                reserved += self.reserve_for_reference(placed.start + shift, block);
                (coverage, generation) =
                    self.wait_for_reference(placed.start + shift, Some(self.reference_deadline(block)));
                reference_wait += waited.elapsed();
            }
            if !self.was_processing || realigned || phase_changed || generation != self.reference_generation {
                self.pipeline.reset();
                retrained = true;
            } else if interrupted
                || generation != self.reference_generation
                || (coverage >= 0.99) != self.reference_complete
            {
                self.pipeline.restart_stream();
                stream_reset = true;
            }
            self.pipeline.process_with_reference(&block.samples, &self.far, &mut self.clean, coverage >= 0.99)?;
        } else {
            self.clean = block.samples;
        }
        self.was_processing = processing;
        self.reference_generation = generation;
        self.reference_complete = coverage >= 0.99;
        let clipped = clipping(&block.samples);
        if clipped {
            self.last_clip = Some(frame);
        }
        let processing_time = started.elapsed() - reference_wait;
        let (underflows, trims, padding_events, waiting) = {
            let mut output = self.output.lock().unwrap();
            if self.config.output.is_some() {
                // Keep the reserve through processing, then replace it with voice under
                // one lock so the renderer cannot observe an empty queue in between.
                output.release_reserve(reserved);
                output.push(&self.clean);
            }
            (output.underflows, output.trims, output.padding_events, output.len())
        };
        let mut stats = self.shared.stats.lock().unwrap();
        let meter = |previous: f32, level: f32| level.max(previous - METER_RELEASE_DB).max(FLOOR_DBFS);
        stats.frames = frame + 1;
        stats.processing = processing;
        stats.options = self.pipeline.options();
        stats.microphone_dbfs = meter(stats.microphone_dbfs, dbfs(&block.samples));
        stats.playback_dbfs = meter(stats.playback_dbfs, dbfs(self.far.as_flattened()));
        stats.output_dbfs = meter(stats.output_dbfs, dbfs(&self.clean));
        self.shared.meters.store(Levels {
            microphone_dbfs: stats.microphone_dbfs,
            playback_dbfs: stats.playback_dbfs,
            output_dbfs: stats.output_dbfs,
        });
        stats.reference_coverage = coverage as f32;
        stats.clipping = self.last_clip.is_some_and(|clip| frame - clip < CLIP_HOLD_FRAMES);
        stats.clipped_frames += u64::from(clipped);
        stats.ai_overloaded = self.pipeline.ai_overloaded();
        stats.dropped_blocks = self.dropped.load(Ordering::Relaxed);
        stats.output_underflows = underflows;
        stats.output_trims = trims;
        stats.output_padding_events = padding_events;
        stats.output_buffer_ms = waiting as f32 * 1000.0 / RATE as f32;
        stats.reference_shift_ms = (self.alignment.shift() * 1000.0) as f32;
        stats.incomplete_reference_frames += u64::from(coverage < 0.99);
        stats.canceller_retrains += u64::from(retrained);
        stats.stream_resets += u64::from(stream_reset);
        stats.reference_discontinuities = generation;
        stats.processing_ms = processing_time.as_secs_f32() * 1000.0;
        stats.reference_wait_ms = reference_wait.as_secs_f32() * 1000.0;
        Ok(())
    }

    /// Fill `far` with the playback from `start`, waiting until `deadline` for it to arrive.
    fn wait_for_reference(&mut self, start: f64, deadline: Option<Instant>) -> (f64, u64) {
        let last = start + (FRAME - 1) as f64 / f64::from(RATE);
        let mut timeline = self.reference.timeline.lock().unwrap();
        if let Some(deadline) = deadline {
            while !timeline.covers(last) && timeline.generation() == self.reference_generation {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                timeline = self.reference.arrived.wait_timeout(timeline, deadline - now).unwrap().0;
            }
        }
        (timeline.frame(start, &mut self.far), timeline.generation())
    }

    fn reference_deadline(&self, block: &MicBlock) -> Instant {
        let deadline = block.arrived + REFERENCE_WAIT + Duration::from_secs_f64(self.alignment.shift().max(0.0));
        if self.config.output.is_none() {
            return deadline;
        }
        let buffered = self.output.lock().unwrap().len() as f64 / f64::from(RATE);
        deadline.min(Instant::now() + Duration::from_secs_f64((buffered - OUTPUT_MARGIN).max(0.0)))
    }

    fn reserve_for_reference(&self, start: f64, block: &MicBlock) -> usize {
        if self.config.output.is_none() {
            return 0;
        }
        let deadline = block.arrived + REFERENCE_WAIT + Duration::from_secs_f64(self.alignment.shift().max(0.0));
        let remaining = deadline.saturating_duration_since(Instant::now()).as_secs_f64();
        if remaining == 0.0 {
            return 0;
        }
        let end = start + FRAME as f64 / f64::from(RATE);
        let missing = {
            let timeline = self.reference.timeline.lock().unwrap();
            if timeline.generation() != self.reference_generation || timeline.covers(end - 1.0 / f64::from(RATE)) {
                return 0;
            }
            timeline.end().map_or(remaining, |latest| (end - latest).max(0.0))
        };
        // Prime only the actual future reference deficit plus delivery/processing jitter.
        // The clock-drift target stays at 20 ms after this one wait consumes the reserve.
        self.output.lock().unwrap().reserve(missing.min(remaining) + REFERENCE_WAIT.as_secs_f64() + OUTPUT_MARGIN)
    }
}

/// Fail if any stream stopped.
fn check(streams: &[Option<&dyn Stream>]) -> Result<(), EngineError> {
    match streams.iter().flatten().find_map(|stream| stream.failure()) {
        Some(failure) => Err(echobridge_audio::Error::System(failure).into()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use crate::{EchoMode, NoiseRemoval};
    use echobridge_audio::fake::{CABLE, FakeBackend, HEADPHONES, MICROPHONE, Script};

    use super::*;

    fn worker() -> Worker {
        let options = Options { echo: EchoMode::CleanVoice, ..Options::default() };
        let config = EngineConfig {
            microphone: MICROPHONE.into(),
            playback: HEADPHONES.into(),
            output: Some(CABLE.into()),
            options,
            processing: true,
        };
        Worker {
            backend: Arc::new(FakeBackend::new(Script::default())),
            config,
            shared: Arc::new(Shared {
                stop: AtomicBool::new(false),
                processing: AtomicBool::new(true),
                stats: Mutex::new(Stats::default()),
                meters: Arc::default(),
                failure: Mutex::new(None),
            }),
            commands: channel().1,
            pipeline: Pipeline::new(options).unwrap(),
            reference: Arc::new(Reference {
                timeline: Mutex::new(ReferenceTimeline::new(RATE)),
                arrived: Condvar::new(),
            }),
            output: Arc::new(Mutex::new(ElasticBuffer::new(RATE))),
            dropped: Arc::default(),
            clock: BlockClock::live(RATE),
            alignment: Alignment::default(),
            was_processing: false,
            reference_generation: 0,
            reference_complete: false,
            last_clip: None,
            far: [[0.0; 2]; FRAME],
            clean: [0.0; FRAME],
        }
    }

    #[test]
    fn a_microphone_stream_interruption_does_not_forget_the_learned_leak() {
        interruption_removal(NoiseRemoval::Off, false);
    }

    #[test]
    fn clean_voice_ai_keeps_the_learned_leak_after_a_microphone_queue_gap() {
        interruption_removal(NoiseRemoval::Ai, true);
    }

    fn interruption_removal(noise: NoiseRemoval, queue_gap: bool) {
        let mut worker = worker();
        let options = Options { echo: EchoMode::CleanVoice, noise, ..Options::default() };
        worker.pipeline.apply(Stages::prepare(options, Some(&worker.pipeline.options())).unwrap());
        let mut seed = 11u32;
        let playback: Vec<Stereo> = (0..600 * FRAME)
            .map(|_| {
                std::array::from_fn(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    0.2 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
                })
            })
            .collect();
        let (mut before, mut after) = (0.0, 0.0);
        for frame in 0..500 {
            let start = frame * FRAME;
            let time = frame as f64 * 0.01;
            worker.reference.timeline.lock().unwrap().append(&playback[start..start + FRAME], time);
            if frame == 400 {
                continue;
            } // one lost microphone block
            let samples = std::array::from_fn(|i| {
                let t = start + i;
                if t >= 45 { 0.5 * playback[t - 45][0] + 0.3 * playback[t - 45][1] } else { 0.0 }
            });
            let block = MicBlock {
                samples,
                time,
                discontinuity: frame == 401 && !queue_gap,
                arrived: Instant::now(),
                skipped: usize::from(frame == 401 && queue_gap),
            };
            worker.process(&block, frame as u64).unwrap();
            if (405..425).contains(&frame) {
                before += samples.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
                after += worker.clean.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            }
        }
        let removed = 10.0 * (before / after.max(1e-20)).log10();
        assert!(removed > 25.0, "a short interruption let the leak return: {removed:.1} dB");
        let stats = worker.shared.stats.lock().unwrap();
        assert_eq!(stats.canceller_retrains, 1);
        assert_eq!(stats.stream_resets, 1);
        if noise == NoiseRemoval::Ai {
            assert!(!stats.ai_overloaded, "the recovery check must keep AI active");
        }
    }

    #[test]
    fn queue_overflow_marks_the_next_accepted_block_and_keeps_device_flags() {
        let (sender, receiver) = sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let mut queue = MicQueue { sender, dropped: dropped.clone(), skipped: 0, discontinuity: false };
        let block = |discontinuity| MicBlock {
            samples: [0.0; FRAME],
            time: 0.0,
            discontinuity,
            arrived: Instant::now(),
            skipped: 0,
        };
        queue.send(block(false));
        queue.send(block(true));
        queue.send(block(false));
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        assert_eq!(receiver.recv().unwrap().skipped, 0);
        queue.send(block(false));
        let resumed = receiver.recv().unwrap();
        assert_eq!(resumed.skipped, 2);
        assert!(resumed.discontinuity);
        queue.send(block(false));
        let next = receiver.recv().unwrap();
        assert_eq!(next.skipped, 0);
        assert!(!next.discontinuity);
    }

    #[test]
    fn an_accepted_queue_gap_preserves_jitter_smoothed_clock_placement() {
        let mut worker = worker();
        let first =
            MicBlock { samples: [0.0; FRAME], time: 1.0, discontinuity: false, arrived: Instant::now(), skipped: 0 };
        worker.process(&first, 0).unwrap();
        let resumed = MicBlock { time: 1.0207, skipped: 1, ..first };
        worker.process(&resumed, 1).unwrap();
        assert!((worker.clock.next().unwrap() - 1.0300014).abs() < 1e-9);
    }

    #[test]
    fn a_changed_microphone_clock_phase_retrains_the_obsolete_path() {
        let mut worker = worker();
        let first =
            MicBlock { samples: [0.0; FRAME], time: 1.0, discontinuity: false, arrived: Instant::now(), skipped: 0 };
        worker.process(&first, 0).unwrap();
        let resumed = MicBlock { time: 1.0207, discontinuity: true, ..first };
        worker.process(&resumed, 1).unwrap();
        assert_eq!(worker.shared.stats.lock().unwrap().canceller_retrains, 2);
    }

    #[test]
    fn a_reference_restart_wakes_a_wait_for_the_previous_timeline() {
        let mut worker = worker();
        worker.reference.timeline.lock().unwrap().append(&[[0.1; 2]; FRAME], 0.0);
        let reference = worker.reference.clone();
        let reset = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let mut timeline = reference.timeline.lock().unwrap();
            timeline.clear();
            timeline.append(&[[0.2; 2]; FRAME], 1.0);
            drop(timeline);
            reference.arrived.notify_all();
        });
        let started = Instant::now();
        let (coverage, generation) = worker.wait_for_reference(5.0, Some(started + Duration::from_secs(1)));
        reset.join().unwrap();
        assert_eq!(coverage, 0.0);
        assert_eq!(generation, 1);
        assert!(started.elapsed() < Duration::from_millis(500), "waited for obsolete reference");
    }
}
