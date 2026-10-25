use std::time::Instant;

use echobridge_denoise::{DELAY, Denoiser, FRAME, Settings};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

const RATE: usize = 48_000;

/// The spoken fixture at 48 kHz and -26 dBFS, repeated to `seconds`.
fn speech(seconds: usize) -> Vec<f32> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/synthetic-speech.wav");
    let mut reader = hound::WavReader::open(path).unwrap();
    let spec = reader.spec();
    let channels = usize::from(spec.channels);
    let samples: Vec<f64> = reader.samples::<i16>().map(|s| f64::from(s.unwrap())).collect();
    let mono: Vec<f64> = samples.chunks_exact(channels).map(|f| f.iter().sum::<f64>() / channels as f64).collect();
    let rate = spec.sample_rate as f64;
    let length = (mono.len() as f64 * RATE as f64 / rate) as usize;
    let resampled: Vec<f64> = (0..length)
        .map(|i| {
            let position = i as f64 * rate / RATE as f64;
            let index = position as usize;
            let next = mono.get(index + 1).copied().unwrap_or(mono[index]);
            mono[index] + (next - mono[index]) * position.fract()
        })
        .collect();
    let voice: Vec<f64> = resampled.iter().cycle().take(seconds * RATE).copied().collect();
    let rms = (voice.iter().map(|x| x * x).sum::<f64>() / voice.len() as f64).sqrt();
    voice.iter().map(|x| (x * 10f64.powf(-26.0 / 20.0) / rms) as f32).collect()
}

fn run(denoiser: &mut Denoiser, signal: &[f32]) -> Vec<f32> {
    let mut output = Vec::with_capacity(signal.len());
    let mut out = [0.0; FRAME];
    for hop in signal.as_chunks::<FRAME>().0 {
        denoiser.process(hop, &mut out);
        output.extend_from_slice(&out);
    }
    output
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|&s| f64::from(s) * f64::from(s)).sum()
}

#[test]
fn removes_hiss_and_keeps_voice_with_fixed_delay() {
    let voice = speech(8);
    let normal = Normal::new(0.0, 10f64.powf(-55.0 / 20.0)).unwrap();
    let mut rng = StdRng::seed_from_u64(2);
    let hiss: Vec<f32> = (0..voice.len()).map(|_| normal.sample(&mut rng) as f32).collect();
    let noisy: Vec<f32> = voice.iter().zip(&hiss).map(|(v, h)| v + h).collect();
    let mut denoiser = Denoiser::new(Settings::default()).unwrap();
    let speech_out = run(&mut denoiser, &noisy);
    let hiss_out = run(&mut denoiser, &hiss);
    assert!(!denoiser.overloaded());

    // Hiss alone, after the model has settled on it.
    let hiss_db = 10.0 * (energy(&hiss_out[2 * RATE..]) / energy(&hiss[2 * RATE..])).log10();
    assert!(hiss_db < -12.0, "hiss {hiss_db:.1} dB");
    // The voice keeps its level at exactly DELAY samples.
    let settled = RATE..voice.len() - DELAY;
    let aligned = &speech_out[DELAY..][settled.clone()];
    let reference = &voice[settled];
    let shared: f64 = aligned.iter().zip(reference).map(|(&a, &r)| f64::from(a) * f64::from(r)).sum();
    let gain_db = 10.0 * (shared / energy(reference)).log10();
    assert!(gain_db > -1.5, "voice {gain_db:.2} dB");
}

#[test]
fn each_hop_runs_well_inside_real_time() {
    let mut denoiser = Denoiser::new(Settings::default()).unwrap();
    let normal = Normal::new(0.0, 0.01).unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    let hop: [f32; FRAME] = std::array::from_fn(|_| normal.sample(&mut rng) as f32);
    let mut out = [0.0; FRAME];
    denoiser.process(&hop, &mut out);
    let started = Instant::now();
    for _ in 0..100 {
        denoiser.process(&hop, &mut out);
    }
    let per_hop = started.elapsed() / 100;
    assert!(per_hop.as_secs_f64() < 0.005, "{per_hop:?} per hop");
}

/// Knocks on a desk: the first one starts after the room has been quiet.
const DESK_KNOCKS_SECONDS: [f64; 6] = [3.0, 3.9, 4.8, 5.1, 6.3, 7.2];
/// When the voice starts after the quiet room and the knocks.
const VOICE_START_SECONDS: usize = 8;
/// How long each knock and its ring are measured for.
const KNOCK_WINDOW: usize = RATE * 3 / 10;

/// Room tone: low rumble plus hiss, with no knocks.
fn room_tone(seconds: usize, seed: u64) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(seed);
    let hiss = Normal::new(0.0, 10f64.powf(-60.0 / 20.0)).unwrap();
    let rumble = Normal::new(0.0, 10f64.powf(-48.0 / 20.0)).unwrap();
    let smoothing = (-2.0 * std::f64::consts::PI * 70.0 / RATE as f64).exp();
    let mut low = 0.0;
    (0..seconds * RATE)
        .map(|_| {
            low = smoothing * low + (1.0 - smoothing) * rumble.sample(&mut rng);
            (low * 6.0 + hiss.sample(&mut rng)) as f32
        })
        .collect()
}

/// One short impact: a few body resonances that ring down within about 100 ms, after a click.
fn knock(peak: f64, seed: u64) -> Vec<f32> {
    // Frequency in Hz, decay time constant in seconds, relative amplitude.
    #[rustfmt::skip]
    const MODES: [(f64, f64, f64); 3] = [
        (95.0, 0.040, 1.0),
        (210.0, 0.028, 0.6),
        (430.0, 0.015, 0.35),
    ];
    let mut rng = StdRng::seed_from_u64(seed);
    let click = Normal::new(0.0, 1.0).unwrap();
    let samples: Vec<f64> = (0..KNOCK_WINDOW)
        .map(|i| {
            let t = i as f64 / RATE as f64;
            let ring: f64 = MODES
                .iter()
                .map(|&(hz, decay, amplitude)| {
                    amplitude * (-t / decay).exp() * (2.0 * std::f64::consts::PI * hz * t).sin()
                })
                .sum();
            ring + 0.8 * (-t / 0.002).exp() * click.sample(&mut rng)
        })
        .collect();
    let top = samples.iter().fold(0.0f64, |a, &s| a.max(s.abs()));
    samples.iter().map(|&s| (s * peak / top) as f32).collect()
}

/// Desk knocks of different strengths over `seconds` of silence.
fn knocks(seconds: usize, onsets: &[f64]) -> Vec<f32> {
    let mut signal = vec![0.0; seconds * RATE];
    for (index, onset) in onsets.iter().enumerate() {
        let start = (onset * RATE as f64) as usize;
        let peak = 0.03 + 0.02 * (index % 3) as f64;
        for (target, sample) in signal[start..].iter_mut().zip(knock(peak, 10 + index as u64)) {
            *target += sample;
        }
    }
    signal
}

fn knock_windows(onsets: &[f64]) -> Vec<std::ops::Range<usize>> {
    onsets
        .iter()
        .map(|onset| {
            let start = (onset * RATE as f64) as usize;
            start..start + KNOCK_WINDOW
        })
        .collect()
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.log10()
}

/// Level change of `output` against `input` over the knock windows, input already aligned.
fn knock_level_change_db(onsets: &[f64], input: &[f32], output: &[f32]) -> f64 {
    let (mut before, mut after) = (0.0, 0.0);
    for window in knock_windows(onsets) {
        before += energy(&input[window.clone()]);
        after += energy(&output[DELAY..][window]);
    }
    db(after / before)
}

/// Gain of the known voice inside `output`, from its correlation with the clean voice.
fn voice_gain_db(voice: &[f32], output: &[f32]) -> f64 {
    let settled = (VOICE_START_SECONDS + 1) * RATE..voice.len() - DELAY;
    let aligned = &output[DELAY..][settled.clone()];
    let reference = &voice[settled];
    let shared: f64 = aligned.iter().zip(reference).map(|(&a, &r)| f64::from(a) * f64::from(r)).sum();
    20.0 * (shared / energy(reference)).log10()
}

#[test]
fn desk_knocks_are_removed_after_the_model_has_settled() {
    let floor = room_tone(9, 21);
    let hits = knocks(9, &DESK_KNOCKS_SECONDS);
    let scene: Vec<f32> = floor.iter().zip(&hits).map(|(f, k)| f + k).collect();
    let mut denoiser = Denoiser::new(Settings::default()).unwrap();
    let output = run(&mut denoiser, &scene);
    assert!(!denoiser.overloaded());

    let change = knock_level_change_db(&DESK_KNOCKS_SECONDS, &scene, &output);
    eprintln!("settled desk knocks: {change:.2} dB");
    assert!(change < -30.0, "knocks changed by {change:.1} dB");
}

/// A known voice that starts after the room has been quiet, with and without desk knocks.
/// Returns the voice and the model output for both rooms.
fn voice_after_quiet_room(level_db: f64, onsets: &[f64]) -> (Vec<f32>, Vec<f32>, Vec<f32>) {
    let gain = 10f32.powf((level_db as f32 + 26.0) / 20.0);
    let start = VOICE_START_SECONDS * RATE;
    let mut voice = vec![0.0; start];
    voice.extend(speech(10).iter().map(|s| s * gain));
    let floor = room_tone(VOICE_START_SECONDS + 10, 22);
    let hits = knocks(VOICE_START_SECONDS + 10, onsets);
    let calm: Vec<f32> = voice.iter().zip(&floor).map(|(v, f)| v + f).collect();
    let knocked: Vec<f32> = calm.iter().zip(&hits).map(|(c, k)| c + k).collect();
    let mut denoiser = Denoiser::new(Settings::default()).unwrap();
    let calm_out = run(&mut denoiser, &calm);
    assert!(!denoiser.overloaded(), "the calm scene overloaded");
    let mut denoiser = Denoiser::new(Settings::default()).unwrap();
    let knocked_out = run(&mut denoiser, &knocked);
    assert!(!denoiser.overloaded());
    (voice, calm_out, knocked_out)
}

#[test]
fn normal_and_quiet_voice_keep_their_level_beside_room_tone() {
    for level_db in [-26.0, -38.0] {
        let (voice, calm_out, _) = voice_after_quiet_room(level_db, &DESK_KNOCKS_SECONDS);
        let gain = voice_gain_db(&voice, &calm_out);
        eprintln!("voice at {level_db} dBFS beside room tone: {gain:.2} dB");
        assert!(gain > -1.5, "voice at {level_db} dBFS changed by {gain:.2} dB");
    }
}

#[test]
fn voice_after_desk_knocks_keeps_its_level_and_the_knocks_are_gone() {
    let hits = knocks(VOICE_START_SECONDS + 10, &DESK_KNOCKS_SECONDS);
    for level_db in [-26.0, -38.0] {
        let (voice, calm_out, knocked_out) = voice_after_quiet_room(level_db, &DESK_KNOCKS_SECONDS);
        let gain = voice_gain_db(&voice, &knocked_out);
        assert!(gain > -1.5, "voice at {level_db} dBFS changed by {gain:.2} dB");

        // What the knocks add to the output, against what they put into the input.
        let (mut added, mut knocked) = (0.0, 0.0);
        for window in knock_windows(&DESK_KNOCKS_SECONDS) {
            let with = &knocked_out[DELAY..][window.clone()];
            let without = &calm_out[DELAY..][window.clone()];
            added += with.iter().zip(without).map(|(&a, &b)| f64::from(a - b).powi(2)).sum::<f64>();
            knocked += energy(&hits[window]);
        }
        let residue = db(added / knocked);
        eprintln!("voice after knocks at {level_db} dBFS: gain {gain:.2} dB, knock residue {residue:.2} dB");
        assert!(residue < -30.0, "voice at {level_db} dBFS leaves {residue:.1} dB of the knocks");
    }
}

#[test]
fn normal_and_quiet_voice_avoid_gross_loss_around_desk_knocks() {
    // Whole-utterance gain does not establish that each struck syllable survives.
    let onsets = DESK_KNOCKS_SECONDS.map(|onset| onset + VOICE_START_SECONDS as f64);
    for level_db in [-26.0, -38.0] {
        let (voice, _, knocked_out) = voice_after_quiet_room(level_db, &onsets);
        let gain = voice_gain_db(&voice, &knocked_out);
        assert!(gain > -1.5, "voice at {level_db} dBFS changed by {gain:.2} dB during knocks");
    }
}
