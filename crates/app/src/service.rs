//! Runs the engine on a control thread, so the window never waits for a device to open
//! or the AI model to load.

use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use echobridge_audio::AudioBackend;
use echobridge_engine::{EchoMode, Engine, EngineConfig, Levels, Meters, NoiseRemoval, Options, Stats};

use crate::diagnostics::{Counters, Health, HealthLog};

/// How often statistics are copied for the window.
const REFRESH: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Phase {
    /// Nothing is sent to the output.
    #[default]
    Off,
    Starting,
    Running,
    /// The engine stopped by itself, for this reason.
    Failed(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub phase: Phase,
    /// The configuration of the running (or last started) engine.
    pub config: Option<EngineConfig>,
    pub stats: Option<Stats>,
}

impl Snapshot {
    pub fn running(&self) -> bool {
        self.phase == Phase::Running
    }

    /// Whether the leak is being removed now.
    pub fn protecting(&self) -> bool {
        self.running() && self.config.as_ref().is_some_and(|c| c.processing)
    }
}

enum Request {
    Start(EngineConfig),
    Stop,
    Options(Options),
    Processing(bool),
    Quit,
}

#[derive(Debug)]
pub struct Service {
    requests: Sender<Request>,
    snapshot: Arc<Mutex<Snapshot>>,
    /// The running engine's meters, read directly so the window can redraw them often.
    meters: Arc<Mutex<Option<Arc<Meters>>>>,
    thread: Option<JoinHandle<()>>,
}

impl Service {
    pub fn new(backend: Arc<dyn AudioBackend>) -> Self {
        let (requests, receiver) = channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let shared = snapshot.clone();
        let meters = Arc::new(Mutex::new(None));
        let live = meters.clone();
        let thread = std::thread::Builder::new()
            .name("EchoBridge control".into())
            .spawn(move || {
                let mut engine: Option<Engine> = None;
                let mut logged = Stats::default();
                let mut health_log = HealthLog::default();
                let update = |change: &dyn Fn(&mut Snapshot)| change(&mut shared.lock().unwrap());
                loop {
                    match receiver.recv_timeout(REFRESH) {
                        Ok(Request::Start(config)) => {
                            stop_engine(&mut engine, &mut health_log);
                            update(&|s| {
                                s.phase = Phase::Starting;
                                s.config = Some(config.clone());
                                s.stats = None;
                            });
                            log::info!("starting: {config:?}");
                            match Engine::start(backend.clone(), config) {
                                Ok(started) => {
                                    logged = Stats::default();
                                    health_log = HealthLog::default();
                                    engine = Some(started);
                                    update(&|s| s.phase = Phase::Running);
                                }
                                Err(error) => {
                                    log::warn!("could not start: {error}");
                                    update(&|s| s.phase = Phase::Failed(error.to_string()));
                                }
                            }
                        }
                        Ok(Request::Stop) => {
                            stop_engine(&mut engine, &mut health_log);
                            log::info!("turned off");
                            update(&|s| {
                                s.phase = Phase::Off;
                                s.stats = None;
                            });
                        }
                        Ok(Request::Options(options)) => {
                            if let Some(running) = &engine {
                                log_health(&mut health_log, &running.stats(), true);
                            }
                            log::info!("options: {options:?}");
                            if let Some(running) = &mut engine
                                && let Err(error) = running.set_options(options)
                            {
                                log::warn!("options not applied: {error}");
                            }
                            update(&|s| {
                                if let Some(config) = &mut s.config {
                                    config.options = options;
                                }
                            });
                        }
                        Ok(Request::Processing(processing)) => {
                            if let Some(running) = &engine {
                                log_health(&mut health_log, &running.stats(), true);
                            }
                            log::info!("{}", if processing { "protecting" } else { "paused" });
                            if let Some(running) = &engine {
                                running.set_processing(processing);
                            }
                            update(&|s| {
                                if let Some(config) = &mut s.config {
                                    config.processing = processing;
                                }
                            });
                        }
                        Ok(Request::Quit) | Err(RecvTimeoutError::Disconnected) => {
                            stop_engine(&mut engine, &mut health_log);
                            return;
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    if let Some(running) = &engine {
                        if let Some(failure) = running.failure() {
                            stop_engine(&mut engine, &mut health_log);
                            log::warn!("stopped: {failure}");
                            update(&|s| {
                                s.phase = Phase::Failed(failure.clone());
                                s.stats = None;
                            });
                        } else {
                            let stats = running.stats();
                            log_health(&mut health_log, &stats, false);
                            log_changes(&logged, &stats);
                            logged = stats.clone();
                            update(&|s| s.stats = Some(stats.clone()));
                        }
                    }
                    *live.lock().unwrap() = engine.as_ref().map(Engine::meters);
                }
            })
            .expect("the control thread starts");
        Self { requests, snapshot, meters, thread: Some(thread) }
    }

    /// Start (or restart) the engine.
    pub fn start(&self, config: EngineConfig) {
        self.requests.send(Request::Start(config)).ok();
    }

    /// Stop the engine; nothing is sent to the output.
    pub fn stop(&self) {
        self.requests.send(Request::Stop).ok();
    }

    pub fn set_options(&self, options: Options) {
        self.requests.send(Request::Options(options)).ok();
    }

    pub fn set_processing(&self, processing: bool) {
        self.requests.send(Request::Processing(processing)).ok();
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().unwrap().clone()
    }

    /// The meter levels now, or `None` while no engine runs.
    pub fn levels(&self) -> Option<Levels> {
        self.meters.lock().unwrap().as_ref().map(|meters| meters.levels())
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.requests.send(Request::Quit).ok();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// Log what went wrong in the audio since the last snapshot: each of these can be heard in
/// a call as a gap, a jump, or the song coming back.
fn log_changes(before: &Stats, now: &Stats) {
    if now.ai_overloaded != before.ai_overloaded {
        log::warn!(
            "AI noise removal {}",
            if now.ai_overloaded { "overloaded: passing audio through" } else { "recovered" }
        );
    }
    if now.reference_shift_ms != before.reference_shift_ms {
        log::info!("leak realigned: reference read {:.1} ms after the microphone", now.reference_shift_ms);
    }
}

/// Join first so the summary includes any frame that was still being processed at stop.
fn stop_engine(engine: &mut Option<Engine>, health_log: &mut HealthLog) {
    if let Some(mut running) = engine.take() {
        running.stop();
        log_health(health_log, &running.stats(), true);
    }
}

fn log_health(logger: &mut HealthLog, stats: &Stats, force: bool) {
    let health = Health {
        counters: Counters {
            frames: stats.frames,
            incomplete_reference: stats.incomplete_reference_frames,
            gaps: stats.output_underflows,
            trims: stats.output_trims,
            dropped: stats.dropped_blocks,
            retrained: stats.canceller_retrains,
            stream_resets: stats.stream_resets,
            reference_discontinuities: stats.reference_discontinuities,
            padding: stats.output_padding_events,
            clipped: stats.clipped_frames,
        },
        processing: stats.processing,
        coverage: stats.reference_coverage,
        reference_shift_ms: stats.reference_shift_ms,
        output_buffer_ms: stats.output_buffer_ms,
        processing_ms: stats.processing_ms,
        reference_wait_ms: stats.reference_wait_ms,
        echo_mode: match stats.options.echo {
            EchoMode::CleanVoice => "clean",
            EchoMode::Adaptive => "adaptive",
            EchoMode::Strong => "strong",
        },
        noise_mode: match stats.options.noise {
            NoiseRemoval::Off => "off",
            NoiseRemoval::Standard => "standard",
            NoiseRemoval::Ai => "ai",
        },
        raw_microphone: stats.raw_microphone,
        audio_priority: stats.audio_priority,
        ai_overloaded: stats.ai_overloaded,
    };
    if let Some(report) = logger.observe(health, std::time::Instant::now(), force) {
        if report.has_problems() {
            log::warn!("{report}");
        } else {
            log::info!("{report}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use echobridge_audio::fake::{CABLE, FakeBackend, HEADPHONES, MICROPHONE, Script};

    use super::*;

    fn wait_for(service: &Service, done: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = service.snapshot();
            if done(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "stuck at {snapshot:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn starts_pauses_and_stops_the_engine() {
        let script = Script {
            microphone: vec![0.0; 48_000 * 10],
            playback: vec![0.0; 2 * 48_000 * 10],
            playback_gain: 1.0,
            tick: Duration::from_millis(5),
            output_failure_after: None,
        };
        let service = Service::new(Arc::new(FakeBackend::new(script)));
        let config = EngineConfig {
            microphone: MICROPHONE.into(),
            playback: HEADPHONES.into(),
            output: Some(CABLE.into()),
            options: Options::default(),
            processing: true,
        };
        service.start(config);
        wait_for(&service, |s| s.protecting() && s.stats.as_ref().is_some_and(|stats| stats.frames > 5));
        service.set_processing(false);
        wait_for(&service, |s| {
            s.running() && !s.protecting() && s.stats.as_ref().is_some_and(|stats| !stats.processing)
        });
        service.stop();
        wait_for(&service, |s| s.phase == Phase::Off);
    }

    #[test]
    fn a_failed_start_reports_why() {
        let service = Service::new(Arc::new(FakeBackend::new(Script::default())));
        service.start(EngineConfig {
            microphone: "missing".into(),
            playback: HEADPHONES.into(),
            output: None,
            options: Options::default(),
            processing: true,
        });
        let snapshot = wait_for(&service, |s| matches!(s.phase, Phase::Failed(_)));
        assert_eq!(snapshot.phase, Phase::Failed("the selected audio device is not connected".into()));
    }
}
