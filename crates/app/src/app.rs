//! The window and the tray icon: how the engine's state, and turn the user's choices
//! into engine commands and saved settings. Everything here runs on the UI thread; the
//! engine, device opening and leak recordings run on their own threads.

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use echobridge_audio::{AudioBackend, Device, system_backend};
use echobridge_dsp::leak::{LeakMeasurement, LeakPath, Verdict, compare_leaks};
use echobridge_engine::{EchoMode, EngineConfig, MAX_DELAY_MS, NoiseRemoval};
use slint::{CloseRequestResponse, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::cli::Launch;
use crate::devices::{self, DeviceLists};
use crate::leak_test::{self, Devices, Progress, Recording, Report, Step};
use crate::platform::{self, SingleInstance};
use crate::service::{Phase, Service};
use crate::settings::{self, Settings};
use crate::status;

/// The code Slint generates from `ui/`.
#[allow(missing_debug_implementations, clippy::all)]
mod ui {
    slint::include_modules!();
}

use ui::{AppTray, AppWindow, LeakTest, Live, Setup};

/// How often the window and tray show new levels and state.
const REFRESH: Duration = Duration::from_millis(100);
/// How often the meters redraw while the window is open: smooth to the eye, cheap to draw.
const METER_REFRESH: Duration = Duration::from_millis(33);
const CABLE_URL: &str = "https://vb-audio.com/Cable/";
const NO_OUTPUT: &str = "Nothing (meters only)";
const ECHO_MODES: [EchoMode; 3] = [EchoMode::CleanVoice, EchoMode::Adaptive, EchoMode::Strong];
const NOISE_MODES: [NoiseRemoval; 3] = [NoiseRemoval::Off, NoiseRemoval::Standard, NoiseRemoval::Ai];

pub fn run(launch: Launch) -> Result<(), Box<dyn Error>> {
    let Some(mut instance) = SingleInstance::acquire(!launch.background)? else {
        return Ok(()); // the running EchoBridge shows its window instead
    };
    let window = AppWindow::new()?;
    let tray = AppTray::new()?;
    let controller = Controller::new(system_backend()?, &window, &tray);
    controller.bind(&window, &tray);
    let ready = controller.refresh_devices();
    if ready {
        controller.start(controller.state.borrow().settings.protect_on_start);
    }

    let has_tray = tray.show().is_ok();
    window.window().on_close_requested(move || {
        if !has_tray {
            slint::quit_event_loop().ok();
        }
        CloseRequestResponse::HideWindow
    });
    if !(launch.background && ready && has_tray) {
        window.show()?;
    }
    let shown = window.as_weak();
    instance.on_show_request(move || {
        let shown = shown.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(window) = shown.upgrade() {
                show(&window);
            }
        })
        .ok();
    });

    let timer = Timer::default();
    let ticking = Rc::downgrade(&controller);
    timer.start(TimerMode::Repeated, REFRESH, move || {
        if let Some(controller) = ticking.upgrade() {
            controller.tick();
        }
    });
    let meter_timer = Timer::default();
    let metering = Rc::downgrade(&controller);
    meter_timer.start(TimerMode::Repeated, METER_REFRESH, move || {
        if let Some(controller) = metering.upgrade() {
            controller.update_meters();
        }
    });
    slint::run_event_loop_until_quit()?;
    // Dropping the controller stops the engine before the process exits.
    drop(timer);
    drop(meter_timer);
    drop(controller);
    Ok(())
}

/// Redraw the whole window: Windows cleared its pixels while it was minimized or hidden.
fn repaint(window: &AppWindow) {
    window.set_repaint(!window.get_repaint());
}

fn show(window: &AppWindow) {
    window.show().ok();
    window.window().set_minimized(false);
    repaint(window);
}

struct Controller {
    backend: Arc<dyn AudioBackend>,
    service: Service,
    window: slint::Weak<AppWindow>,
    tray: slint::Weak<AppTray>,
    settings_path: PathBuf,
    state: RefCell<State>,
    /// The window was minimized at the last meter update.
    was_minimized: Cell<bool>,
}

struct State {
    settings: Settings,
    devices: DeviceLists,
    leak: LeakSession,
}

/// The leak test's steps so far.
#[derive(Default)]
struct LeakSession {
    recording: Option<(Recording, Devices)>,
    worn: Option<(LeakMeasurement, Devices)>,
    covered: Option<(LeakMeasurement, Devices)>,
    verdict: Option<Verdict>,
}

impl Controller {
    fn new(backend: Arc<dyn AudioBackend>, window: &AppWindow, tray: &AppTray) -> Rc<Self> {
        let settings_path = settings::settings_path();
        let settings = settings::load(&settings_path);
        Rc::new(Self {
            service: Service::new(backend.clone()),
            backend,
            window: window.as_weak(),
            tray: tray.as_weak(),
            settings_path,
            state: RefCell::new(State { settings, devices: DeviceLists::default(), leak: LeakSession::default() }),
            was_minimized: Cell::new(false),
        })
    }

    /// A UI callback that reaches the controller while it exists.
    fn act(self: &Rc<Self>, action: impl Fn(&Self) + 'static) -> impl Fn() + 'static {
        let weak: Weak<Self> = Rc::downgrade(self);
        move || {
            if let Some(this) = weak.upgrade() {
                action(&this);
            }
        }
    }

    fn act_with<A>(self: &Rc<Self>, action: impl Fn(&Self, A) + 'static) -> impl Fn(A) + 'static {
        let weak: Weak<Self> = Rc::downgrade(self);
        move |argument| {
            if let Some(this) = weak.upgrade() {
                action(&this, argument);
            }
        }
    }

    fn bind(self: &Rc<Self>, window: &AppWindow, tray: &AppTray) {
        let settings = self.state.borrow().settings.clone();

        let live = window.global::<Live>();
        live.set_echo_mode(index_of(&ECHO_MODES, settings.echo));
        live.set_noise_mode(index_of(&NOISE_MODES, settings.noise));
        live.on_primary_action(self.act(Self::toggle_protection));
        live.on_turn_off(self.act(|this| this.service.stop()));
        live.on_echo_mode_changed(self.act_with(|this, index: i32| {
            this.change_settings(|s| s.echo = ECHO_MODES.get(index as usize).copied().unwrap_or_default());
        }));
        live.on_noise_mode_changed(self.act_with(|this, index: i32| {
            this.change_settings(|s| s.noise = NOISE_MODES.get(index as usize).copied().unwrap_or_default());
        }));

        let setup = window.global::<Setup>();
        setup.set_version(env!("CARGO_PKG_VERSION").into());
        setup.set_autostart(platform::autostart_enabled());
        setup.set_autostart_supported(platform::AUTOSTART_SUPPORTED);
        setup.set_autostart_title(platform::AUTOSTART_TITLE.into());
        setup.set_protect_on_start(settings.protect_on_start);
        setup.set_delay_ms(settings.delay_ms as i32);
        setup.set_max_delay_ms(MAX_DELAY_MS as i32);
        setup.on_devices_changed(self.act(Self::devices_changed));
        setup.on_refresh_devices(self.act(|this| {
            this.refresh_devices();
        }));
        setup.on_get_cable(|| {
            if let Err(error) = platform::open(CABLE_URL) {
                log::warn!("could not open {CABLE_URL}: {error}");
            }
        });
        setup.on_autostart_changed(self.act_with(|this, on: bool| {
            if let Err(error) = platform::set_autostart(on) {
                log::warn!("starting at sign-in not changed: {error}");
            }
            if let Some(window) = this.window.upgrade() {
                window.global::<Setup>().set_autostart(platform::autostart_enabled());
            }
        }));
        setup.on_protect_on_start_changed(
            self.act_with(|this, on: bool| this.change_settings(|s| s.protect_on_start = on)),
        );
        setup.on_delay_changed(self.act_with(|this, delay: i32| {
            this.change_settings(|s| s.delay_ms = delay.clamp(0, MAX_DELAY_MS as i32) as u32);
        }));

        let leak = window.global::<LeakTest>();
        leak.set_seconds(leak_test::STEP_SECONDS as i32);
        leak.on_record(
            self.act_with(|this, step: i32| this.record_leak(if step == 0 { Step::Worn } else { Step::Covered })),
        );
        leak.on_cancel(self.act(Self::cancel_leak));
        leak.on_save_report(self.act(Self::save_leak_report));
        leak.on_open_reports(|| {
            let folder = reports_folder();
            if let Err(error) = std::fs::create_dir_all(&folder).and_then(|()| platform::open_folder(&folder)) {
                log::warn!("could not open the reports folder: {error}");
            }
        });

        let shown = window.as_weak();
        tray.on_open(move || {
            if let Some(window) = shown.upgrade() {
                show(&window);
            }
        });
        tray.on_toggle(self.act(Self::toggle_protection));
        tray.on_quit(|| {
            slint::quit_event_loop().ok();
        });
    }

    /// Reload the device lists and select the saved devices. Returns whether every saved
    /// device is connected, so protection can resume without the user.
    fn refresh_devices(&self) -> bool {
        let Some(window) = self.window.upgrade() else { return false };
        let setup = window.global::<Setup>();
        let lists = match DeviceLists::load(self.backend.as_ref()) {
            Ok(lists) => {
                setup.set_device_error(SharedString::new());
                lists
            }
            Err(error) => {
                setup.set_device_error(format!("Audio devices could not be listed: {error}").into());
                DeviceLists::default()
            }
        };
        let mut state = self.state.borrow_mut();
        let settings = &state.settings;
        let microphone =
            devices::pick(&lists.microphones, settings.microphone.as_ref(), lists.default_microphone.as_ref());
        let playback = devices::pick(&lists.playback, settings.playback.as_ref(), lists.default_playback.as_ref());
        // Before the first setup, the first cable is the natural output; afterwards no
        // saved output means the user chose meters only.
        let output = if settings.output.is_none() && settings.microphone.is_some() {
            None
        } else {
            devices::pick(&lists.outputs, settings.output.as_ref(), None)
        };
        let ready = devices::connected(&lists.microphones, settings.microphone.as_ref())
            && devices::connected(&lists.playback, settings.playback.as_ref())
            && (settings.output.is_none() || devices::connected(&lists.outputs, settings.output.as_ref()));

        let mut outputs = names(&lists.outputs);
        outputs.push(NO_OUTPUT.into());
        setup.set_microphones(model(names(&lists.microphones)));
        setup.set_playback_devices(model(names(&lists.playback)));
        setup.set_outputs(model(outputs));
        setup.set_microphone_index(microphone.map_or(-1, |i| i as i32));
        setup.set_playback_index(playback.map_or(-1, |i| i as i32));
        setup.set_output_index(output.unwrap_or(lists.outputs.len()) as i32);
        setup.set_cable_missing(lists.outputs.is_empty());
        state.devices = lists;
        drop(state);
        self.remember_devices(&setup);
        ready
    }

    /// The user picked another device: save it, and reopen the devices if running.
    fn devices_changed(&self) {
        let Some(window) = self.window.upgrade() else { return };
        self.remember_devices(&window.global::<Setup>());
        let snapshot = self.service.snapshot();
        if matches!(snapshot.phase, Phase::Running | Phase::Starting) {
            self.start(snapshot.config.is_none_or(|c| c.processing));
        }
    }

    /// Save the selected devices, with their current ids.
    fn remember_devices(&self, setup: &Setup<'_>) {
        let (microphone, playback, output) = {
            let state = self.state.borrow();
            let lists = &state.devices;
            let chosen = |devices: &[Device], index: i32| devices.get(index as usize).map(devices::saved);
            (
                chosen(&lists.microphones, setup.get_microphone_index()),
                chosen(&lists.playback, setup.get_playback_index()),
                chosen(&lists.outputs, setup.get_output_index()),
            )
        };
        let output_name = output.as_ref().map(|o| devices::call_app_microphone(&o.name)).unwrap_or_default();
        setup.set_call_app_microphone(output_name.into());
        if microphone.is_none() || playback.is_none() {
            return; // nothing to remember yet
        }
        self.change_settings(|s| {
            s.microphone = microphone;
            s.playback = playback;
            s.output = output;
        });
    }

    /// The engine configuration for the selected devices.
    fn selected_config(&self, processing: bool) -> Option<EngineConfig> {
        let window = self.window.upgrade()?;
        let setup = window.global::<Setup>();
        let state = self.state.borrow();
        let lists = &state.devices;
        Some(EngineConfig {
            microphone: lists.microphones.get(setup.get_microphone_index() as usize)?.id.clone(),
            playback: lists.playback.get(setup.get_playback_index() as usize)?.id.clone(),
            output: lists.outputs.get(setup.get_output_index() as usize).map(|d| d.id.clone()),
            options: state.settings.options(),
            processing,
        })
    }

    fn start(&self, processing: bool) {
        if let Some(config) = self.selected_config(processing) {
            self.service.start(config);
        }
    }

    /// Start when off, pause while protecting, resume while paused.
    fn toggle_protection(&self) {
        let snapshot = self.service.snapshot();
        match snapshot.phase {
            Phase::Running => self.service.set_processing(!snapshot.protecting()),
            Phase::Starting => {}
            Phase::Off | Phase::Failed(_) => self.start(true),
        }
    }

    /// Change and save settings; option changes reach the running engine.
    fn change_settings(&self, change: impl FnOnce(&mut Settings)) {
        let mut state = self.state.borrow_mut();
        let before = state.settings.options();
        change(&mut state.settings);
        let options = state.settings.options();
        if let Err(error) = settings::save(&self.settings_path, &state.settings) {
            log::warn!("settings not saved: {error}");
        }
        if options != before {
            self.service.set_options(options);
        }
    }

    fn tick(&self) {
        let (Some(window), Some(tray)) = (self.window.upgrade(), self.tray.upgrade()) else { return };
        let snapshot = self.service.snapshot();
        let setup = window.global::<Setup>();
        let (microphone, playback, output, call_app) = {
            let state = self.state.borrow();
            let lists = &state.devices;
            let name = |devices: &[Device], index: i32| devices.get(index as usize).map(|d| d.name.clone());
            // The call app hears the output the engine was started with.
            let running_output = snapshot.config.as_ref().and_then(|c| c.output.as_ref());
            let call_app = running_output
                .and_then(|id| lists.outputs.iter().find(|d| &d.id == id))
                .map(|d| devices::call_app_microphone(&d.name));
            (
                name(&lists.microphones, setup.get_microphone_index()).unwrap_or_default(),
                name(&lists.playback, setup.get_playback_index()).unwrap_or_default(),
                name(&lists.outputs, setup.get_output_index()).unwrap_or_else(|| NO_OUTPUT.to_string()),
                call_app,
            )
        };
        let status = status::describe(&snapshot, call_app.as_deref());

        let live = window.global::<Live>();
        live.set_tone(tone(status.tone));
        live.set_headline(status.headline.into());
        live.set_detail(status.detail.into());
        live.set_running(snapshot.running());
        live.set_starting(snapshot.phase == Phase::Starting);
        live.set_protecting(snapshot.protecting());
        live.set_can_start(!microphone.is_empty() && !playback.is_empty());
        live.set_microphone(fallback(&microphone, "No microphone").into());
        live.set_playback(fallback(&playback, "No headphones").into());
        live.set_output(output.into());
        live.set_clipping(snapshot.stats.as_ref().is_some_and(|s| s.clipping));

        tray.set_running(snapshot.phase == Phase::Running);
        tray.set_protecting(snapshot.protecting());
        tray.set_status(status.headline.into());

        let leak = window.global::<LeakTest>();
        leak.set_microphone(fallback(&microphone, "none selected").into());
        leak.set_playback(fallback(&playback, "none selected").into());
        self.poll_leak(&leak);
    }

    /// Show the engine's levels as they are now; separate from [`Self::tick`] so the bars
    /// follow the voice closely.
    fn update_meters(&self) {
        let Some(window) = self.window.upgrade() else { return };
        let minimized = window.window().is_minimized();
        if self.was_minimized.replace(minimized) && !minimized {
            repaint(&window);
        }
        if !window.window().is_visible() {
            return;
        }
        let levels = self.service.levels().unwrap_or_default();
        let live = window.global::<Live>();
        live.set_microphone_level(levels.microphone_dbfs);
        live.set_playback_level(levels.playback_dbfs);
        live.set_output_level(levels.output_dbfs);
    }

    fn record_leak(&self, step: Step) {
        let Some(config) = self.selected_config(false) else { return };
        let Some(window) = self.window.upgrade() else { return };
        let mut state = self.state.borrow_mut();
        if state.leak.recording.is_some() {
            return;
        }
        let find = |devices: &[Device], id: &str| {
            devices.iter().find(|d| d.id == id).map_or(String::new(), |d| d.name.clone())
        };
        let devices = Devices {
            microphone: find(&state.devices.microphones, &config.microphone),
            playback: find(&state.devices.playback, &config.playback),
        };
        let leak = window.global::<LeakTest>();
        if step == Step::Worn {
            state.leak.covered = None;
            leak.set_covered_result("Not recorded yet.".into());
        }
        state.leak.verdict = None;
        leak.set_has_verdict(false);
        leak.set_saved_path(SharedString::new());
        leak.set_progress(0.0);
        leak.set_recording(step_index(step));
        let duration = Duration::from_secs(leak_test::STEP_SECONDS);
        let recording = Recording::start(self.backend.clone(), config.microphone, config.playback, step, duration);
        state.leak.recording = Some((recording, devices));
    }

    fn cancel_leak(&self) {
        let cancelled = self.state.borrow_mut().leak.recording.take();
        if let (Some((recording, _)), Some(window)) = (cancelled, self.window.upgrade()) {
            let leak = window.global::<LeakTest>();
            set_step_result(&leak, recording.step, "Cancelled.");
            leak.set_recording(-1);
            // Dropping the recording stops it.
        }
    }

    fn poll_leak(&self, leak: &LeakTest<'_>) {
        let mut state = self.state.borrow_mut();
        let session = &mut state.leak;
        let Some((recording, _)) = &session.recording else { return };
        let step = recording.step;
        let text = match recording.progress() {
            Progress::Recording(fraction) => {
                leak.set_progress(fraction);
                return;
            }
            Progress::Measured(measurement) => {
                let text = leak_test::describe(&measurement);
                let (_, devices) = session.recording.take().expect("a recording is running");
                match step {
                    Step::Worn => session.worn = Some((measurement, devices)),
                    Step::Covered => session.covered = Some((measurement, devices)),
                }
                text
            }
            Progress::Failed(reason) => {
                session.recording = None;
                format!("Recording failed: {reason}")
            }
            Progress::Cancelled => {
                session.recording = None;
                "Cancelled.".into()
            }
        };
        set_step_result(leak, step, &text);
        leak.set_recording(-1);
        leak.set_worn_done(session.worn.is_some());

        if let (Some((worn, worn_devices)), Some((covered, covered_devices))) = (&session.worn, &session.covered) {
            let verdict = compare_leaks(worn, covered);
            let mut detail = leak_test::verdict_detail(&verdict);
            if worn_devices != covered_devices {
                detail.push_str("\n\nThe two steps used different devices; repeat them with the same setup.");
            }
            leak.set_verdict_title(verdict.title.into());
            leak.set_verdict_detail(detail.into());
            leak.set_verdict_tone(match verdict.path {
                LeakPath::None => ui::Tone::Good,
                LeakPath::Acoustic => ui::Tone::Busy,
                LeakPath::Electrical | LeakPath::Mixed => ui::Tone::Warning,
            });
            leak.set_has_verdict(true);
            session.verdict = Some(verdict);
        }
    }

    fn save_leak_report(&self) {
        let state = self.state.borrow();
        let session = &state.leak;
        let (Some((worn, devices)), Some((covered, _)), Some(verdict)) =
            (&session.worn, &session.covered, &session.verdict)
        else {
            return;
        };
        let saved = match Report::new(devices, worn, covered, verdict).save(&reports_folder()) {
            Ok(path) => path.display().to_string(),
            Err(error) => format!("nowhere: {error}"),
        };
        if let Some(window) = self.window.upgrade() {
            window.global::<LeakTest>().set_saved_path(saved.into());
        }
    }
}

fn reports_folder() -> PathBuf {
    settings::data_dir().join("reports")
}

fn tone(tone: status::Tone) -> ui::Tone {
    match tone {
        status::Tone::Idle => ui::Tone::Idle,
        status::Tone::Busy => ui::Tone::Busy,
        status::Tone::Good => ui::Tone::Good,
        status::Tone::Warning => ui::Tone::Warning,
        status::Tone::Error => ui::Tone::Error,
    }
}

fn step_index(step: Step) -> i32 {
    match step {
        Step::Worn => 0,
        Step::Covered => 1,
    }
}

fn set_step_result(leak: &LeakTest<'_>, step: Step, text: &str) {
    match step {
        Step::Worn => leak.set_worn_result(text.into()),
        Step::Covered => leak.set_covered_result(text.into()),
    }
}

fn index_of<T: PartialEq>(all: &[T], value: T) -> i32 {
    all.iter().position(|v| *v == value).map_or(0, |i| i as i32)
}

fn names(devices: &[Device]) -> Vec<SharedString> {
    devices.iter().map(|d| SharedString::from(d.name.as_str())).collect()
}

fn model(items: Vec<SharedString>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(items))
}

fn fallback<'a>(text: &'a str, otherwise: &'a str) -> &'a str {
    if text.is_empty() { otherwise } else { text }
}
