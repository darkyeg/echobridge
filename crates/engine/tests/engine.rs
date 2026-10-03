//! The live engine on scripted devices, five times faster than real time.

use std::sync::Arc;
use std::time::{Duration, Instant};

use echobridge_audio::fake::{CABLE, FakeBackend, HEADPHONES, MICROPHONE, Script};
use echobridge_engine::{EchoMode, Engine, EngineConfig, EngineError, NoiseRemoval, Options};

const RATE: usize = 48_000;
const TICK: Duration = Duration::from_millis(2);

/// Stereo noise playback and its wired leak: a 0.94 ms delay, mixed from both channels.
fn script(seconds: usize, voice: bool) -> Script {
    let mut state = 11u32;
    let mut noise = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let total = seconds * RATE;
    let playback: Vec<f32> = (0..2 * total).map(|_| 0.2 * noise()).collect();
    let microphone = (0..total)
        .map(|t| {
            let leak = if t >= 45 { 0.5 * playback[2 * (t - 45)] + 0.3 * playback[2 * (t - 45) + 1] } else { 0.0 };
            let speech = if voice { 0.05 * (t as f32 * 0.05).sin() } else { 0.0 };
            leak + speech
        })
        .collect();
    Script { microphone, playback, playback_gain: 1.0, tick: TICK, output_failure_after: None }
}

fn config(options: Options, processing: bool) -> EngineConfig {
    EngineConfig {
        microphone: MICROPHONE.into(),
        playback: HEADPHONES.into(),
        output: Some(CABLE.into()),
        options,
        processing,
    }
}

/// Wait until the backend rendered `seconds` of audio, or fail after a generous timeout.
fn wait_for_output(backend: &FakeBackend, seconds: usize) -> Vec<f32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let rendered = backend.rendered.lock().unwrap().clone();
        if rendered.len() >= seconds * RATE {
            return rendered;
        }
        assert!(Instant::now() < deadline, "only {} samples rendered", rendered.len());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / x.len() as f64
}

#[test]
fn a_reference_shift_near_the_limit_keeps_coverage_and_removal() {
    let mut script = script(10, false);
    // Run this timing regression at real cadence: accelerated capture would hide a
    // 100 ms reference delay inside the old 20 ms wall-clock wait budget.
    script.tick = Duration::from_millis(10);
    let advance = RATE * 97 / 1000;
    script.microphone = (0..script.microphone.len())
        .map(|t| {
            let offset = 2 * (t + advance);
            script.playback.get(offset..offset + 2).map_or(0.0, |s| 0.5 * s[0] + 0.3 * s[1])
        })
        .collect();
    let leak = energy(&script.microphone[7 * RATE..9 * RATE]);
    let backend = Arc::new(FakeBackend::new(script));
    let options = Options { echo: EchoMode::CleanVoice, ..Options::default() };
    let engine = Engine::start(backend.clone(), config(options, true)).unwrap();
    wait_for_output(&backend, 7);
    let before = engine.stats().incomplete_reference_frames;
    let rendered = wait_for_output(&backend, 9);
    let stats = engine.stats();
    assert!(stats.reference_shift_ms > 90.0, "shift {:.1} ms", stats.reference_shift_ms);
    assert!(stats.reference_coverage > 0.99, "coverage {}", stats.reference_coverage);
    assert!(stats.incomplete_reference_frames - before < 10, "reference remained incomplete: {stats:?}");
    let removed = 10.0 * (leak / energy(&rendered[7 * RATE..9 * RATE])).log10();
    assert!(removed > 25.0, "removed {removed:.1} dB");
    assert_eq!(engine.failure(), None);
}

#[test]
fn clean_voice_removes_the_leak_on_live_streams() {
    let script = script(8, false);
    let leak = energy(&script.microphone[5 * RATE..7 * RATE]);
    let backend = Arc::new(FakeBackend::new(script));
    let options = Options { echo: EchoMode::CleanVoice, ..Options::default() };
    let engine = Engine::start(backend.clone(), config(options, true)).unwrap();
    let rendered = wait_for_output(&backend, 7);
    let removed = 10.0 * (leak / energy(&rendered[5 * RATE..7 * RATE])).log10();
    assert!(removed > 25.0, "removed {removed:.1} dB");
    let stats = engine.stats();
    assert!(stats.raw_microphone && stats.processing);
    assert_eq!(stats.dropped_blocks, 0);
    assert!(stats.reference_coverage > 0.99);
    assert_eq!(engine.failure(), None);
}

#[test]
fn pass_through_sends_the_microphone_unchanged() {
    let script = script(4, true);
    let microphone = energy(&script.microphone[2 * RATE..3 * RATE]);
    let backend = Arc::new(FakeBackend::new(script));
    let engine = Engine::start(backend.clone(), config(Options::default(), false)).unwrap();
    let rendered = wait_for_output(&backend, 3);
    // A loaded test machine can starve the output. Its gaps are exact zeros, which the noisy
    // microphone never produces, and can cover part of a block, so only blocks without any
    // gap are compared.
    let delivered: Vec<f32> = rendered[2 * RATE..3 * RATE]
        .chunks(480)
        .filter(|block| block.iter().all(|&s| s != 0.0))
        .flatten()
        .copied()
        .collect();
    assert!(delivered.len() > RATE / 2, "most of the output was silent");
    let change = 10.0 * (energy(&delivered) / microphone).log10();
    assert!(change.abs() < 0.5, "level changed by {change:.2} dB");
    assert!(!engine.stats().processing);
}

#[test]
fn options_change_while_running() {
    let backend = Arc::new(FakeBackend::new(script(6, true)));
    let mut engine = Engine::start(backend.clone(), config(Options::default(), true)).unwrap();
    for options in [
        Options { echo: EchoMode::CleanVoice, noise: NoiseRemoval::Ai, delay_ms: 0 },
        Options { echo: EchoMode::Strong, noise: NoiseRemoval::Ai, delay_ms: 0 },
        Options { echo: EchoMode::Adaptive, noise: NoiseRemoval::Standard, delay_ms: 20 },
    ] {
        engine.set_options(options).unwrap();
        let frames = engine.stats().frames;
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.stats().options != options || engine.stats().frames < frames + 20 {
            assert!(Instant::now() < deadline, "options {options:?} never applied");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    engine.set_processing(false);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!engine.stats().processing);
    assert_eq!(engine.failure(), None);
}

#[test]
fn a_missing_device_fails_to_start() {
    let backend = Arc::new(FakeBackend::new(script(1, false)));
    let mut config = config(Options::default(), true);
    config.microphone = "unplugged".into();
    let error = Engine::start(backend, config).unwrap_err();
    assert!(matches!(error, EngineError::Audio(echobridge_audio::Error::DeviceNotFound)), "{error}");
}

#[test]
fn the_output_cannot_be_the_playback_device() {
    let backend = Arc::new(FakeBackend::new(script(1, false)));
    let mut config = config(Options::default(), true);
    config.output = Some(HEADPHONES.into());
    assert!(matches!(Engine::start(backend, config), Err(EngineError::OutputIsPlayback)));
}

#[test]
fn an_output_failure_stops_the_engine_with_its_reason() {
    let mut script = script(6, false);
    script.output_failure_after = Some(50);
    let backend = Arc::new(FakeBackend::new(script));
    let engine = Engine::start(backend, config(Options::default(), true)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while engine.failure().is_none() {
        assert!(Instant::now() < deadline, "the failure was not reported");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(engine.failure().unwrap(), "The output device was disconnected.");
}

#[test]
fn the_playback_volume_scales_the_reference() {
    let mut script = script(4, false);
    script.playback_gain = 0.5;
    let backend = Arc::new(FakeBackend::new(script));
    let engine = Engine::start(backend.clone(), config(Options::default(), true)).unwrap();
    wait_for_output(&backend, 2);
    // The playback is uniform noise of RMS 0.2 / sqrt(12) = -24.8 dBFS; half volume is
    // 6 dB lower. The meter shows recent peaks, so allow for its ballistics.
    let level = engine.stats().playback_dbfs;
    assert!((level + 30.8).abs() < 1.5, "playback meter {level:.1} dBFS");
}
