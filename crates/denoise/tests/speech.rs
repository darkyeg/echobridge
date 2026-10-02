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
