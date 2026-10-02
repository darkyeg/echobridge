//! Runs the engine on a control thread, so the window never waits for a device to open
//! or the AI model to load.

use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use echobridge_audio::AudioBackend;
use echobridge_engine::{Engine, EngineConfig, Levels, Meters, Options, Stats};

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
                let update = |change: &dyn Fn(&mut Snapshot)| change(&mut shared.lock().unwrap());
                loop {
                    match receiver.recv_timeout(REFRESH) {
                        Ok(Request::Start(config)) => {
                            engine = None; // close the old devices before opening new ones
                            update(&|s| {
                                s.phase = Phase::Starting;
                                s.config = Some(config.clone());
                                s.stats = None;
                            });
                            log::info!("starting: {config:?}");
                            match Engine::start(backend.clone(), config) {
                                Ok(started) => {
                                    logged = Stats::default();
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
                            log::info!("turned off");
                            engine = None;
                            update(&|s| {
                                s.phase = Phase::Off;
                                s.stats = None;
                            });
                        }
                        Ok(Request::Options(options)) => {
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
                        Ok(Request::Quit) | Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    if let Some(running) = &engine {
                        if let Some(failure) = running.failure() {
                            log::warn!("stopped: {failure}");
                            engine = None;
                            update(&|s| {
                                s.phase = Phase::Failed(failure.clone());
                                s.stats = None;
                            });
                        } else {
                            let stats = running.stats();
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
    let counters = [
        ("output gaps (call app heard silence)", before.output_underflows, now.output_underflows),
        ("output trims (audio skipped to cut delay)", before.output_trims, now.output_trims),
        ("microphone blocks dropped (processing fell behind)", before.dropped_blocks, now.dropped_blocks),
        (
            "frames with incomplete playback reference",
            before.incomplete_reference_frames,
            now.incomplete_reference_frames,
        ),
    ];
    for (what, before, now) in counters {
        if now > before {
            log::warn!("{what}: +{} (total {now})", now - before);
        }
    }
    if now.ai_overloaded != before.ai_overloaded {
        log::warn!(
            "AI noise removal {}",
            if now.ai_overloaded { "overloaded: passing audio through" } else { "recovered" }
        );
    }
    if now.reference_shift_ms != before.reference_shift_ms {
        log::info!("leak realigned: reference read {:.1} ms after the microphone", now.reference_shift_ms);
    }
    if now.clipping && !before.clipping {
        log::info!("microphone clipping");
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
