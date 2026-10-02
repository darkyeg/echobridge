//! Synthetic signals shared by the integration tests.
#![allow(dead_code)]

use echobridge_dsp::RATE;
use echobridge_dsp::fft::RealFft;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};
use realfft::num_complex::Complex64;

pub const R: usize = RATE as usize;

pub fn rng(seed: u64) -> StdRng {
    StdRng::seed_from_u64(seed)
}

pub fn normal(rng: &mut StdRng, sigma: f64, count: usize) -> Vec<f64> {
    let distribution = Normal::new(0.0, sigma).unwrap();
    (0..count).map(|_| distribution.sample(rng)).collect()
}

/// Mean power in dB.
pub fn db(x: &[f64]) -> f64 {
    10.0 * (x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64 + 1e-30).log10()
}

pub fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// `numpy.convolve(signal, kernel)[:len(signal)]`.
pub fn convolve(signal: &[f64], kernel: &[f64]) -> Vec<f64> {
    let mut out = vec![0.0; signal.len()];
    for (i, o) in out.iter_mut().enumerate() {
        let taps = kernel.len().min(i + 1);
        *o = (0..taps).map(|k| kernel[k] * signal[i - k]).sum();
    }
    out
}

pub fn to_f32(x: &[f64]) -> Vec<f32> {
    x.iter().map(|&s| s as f32).collect()
}

pub fn interleave(left: &[f64], right: &[f64]) -> Vec<f32> {
    left.iter().zip(right).flat_map(|(&l, &r)| [l as f32, r as f32]).collect()
}

/// A 10 ms electrical response per channel: a direct path 0.94 ms after playback and a
/// decaying tail, at -5 dB.
pub fn jack_response(rng: &mut StdRng) -> [Vec<f64>; 2] {
    let make = |rng: &mut StdRng| {
        let noise = normal(rng, 1.0, 480);
        let mut response: Vec<f64> = (0..480).map(|i| (-(i as f64) / 60.0).exp() * noise[i]).collect();
        response[45] += 4.0;
        let norm = response.iter().map(|x| x * x).sum::<f64>().sqrt();
        let scale = 10f64.powf(-5.0 / 20.0) / norm;
        response.iter_mut().for_each(|x| *x *= scale);
        response
    };
    [make(rng), make(rng)]
}

pub struct Leak {
    pub music: [Vec<f64>; 2],
    pub leak: Vec<f64>,
    pub voice: Vec<f64>,
    pub talking: Vec<bool>,
}

/// Stereo noise music leaking through the jack, with optional speech-like bursts.
pub fn jack_leak(seconds: usize, talk_from: Option<usize>) -> Leak {
    let mut rng = rng(8);
    let count = R * seconds;
    let left = normal(&mut rng, 0.1, count);
    let other = normal(&mut rng, 0.1, count);
    // Correlated channels, as in songs.
    let right: Vec<f64> = left.iter().zip(&other).map(|(l, o)| 0.7 * l + 0.3 * o).collect();
    let response = jack_response(&mut rng);
    let leak: Vec<f64> =
        convolve(&left, &response[0]).iter().zip(convolve(&right, &response[1])).map(|(a, b)| a + b).collect();
    let mut voice = vec![0.0; count];
    let mut talking = vec![false; count];
    if let Some(from) = talk_from {
        let noise = normal(&mut rng, 0.05, count);
        let mut start = from * R;
        while start < count - 2 * R {
            for i in start..start + 2 * R {
                talking[i] = true;
                let envelope = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 4.0 * i as f64 / R as f64).sin();
                voice[i] = noise[i] * envelope;
            }
            start += (3.5 * R as f64) as usize;
        }
    }
    Leak { music: [left, right], leak, voice, talking }
}

/// A song with a sung melody leaks electrically, and the user sings the same melody from
/// 15 s to 30 s, a little off in pitch and timing as people sing along.
pub fn sing_along(seconds: usize) -> Leak {
    let mut rng = rng(5);
    let count = R * seconds;
    let t: Vec<f64> = (0..count).map(|i| i as f64 / R as f64).collect();
    let scale = [0, 2, 4, 5, 7, 9, 11, 12];
    let notes: Vec<f64> = (0..seconds * 3).map(|_| f64::from(scale[rng.gen_range(0..scale.len())])).collect();
    let mut fft = RealFft::new(count);

    let mut singer = |onsets: &[f64], cents: &[f64], vibrato: f64, formants: &[(f64, f64)]| -> Vec<f64> {
        let mut phase = 0.0;
        let mut source = vec![0.0; count];
        let mut envelope = vec![0.0; count];
        let mut index = 0usize;
        for i in 0..count {
            while index + 1 < onsets.len() && onsets[index + 1] <= t[i] {
                index += 1;
            }
            let f0 = 220.0
                * 2f64.powf((notes[index] + cents[index] / 100.0) / 12.0)
                * (1.0 + 0.006 * (2.0 * std::f64::consts::PI * vibrato * t[i]).sin());
            phase += 2.0 * std::f64::consts::PI * f0 / R as f64;
            source[i] = (1..25).map(|k| (k as f64 * phase).sin() / k as f64).sum();
            let position = ((t[i] - onsets[index]) * 3.0).rem_euclid(1.0);
            envelope[i] = ((std::f64::consts::PI * position).sin() * 3.0).clamp(0.0, 1.0);
        }
        // A vowel: resonances applied to the whole signal at once.
        let mut spectrum = vec![Complex64::default(); fft.bins()];
        fft.forward(&source, &mut spectrum);
        for (k, s) in spectrum.iter_mut().enumerate() {
            let frequency = k as f64 * R as f64 / count as f64;
            let shape: f64 =
                formants.iter().map(|(centre, width)| 1.0 / (1.0 + ((frequency - centre) / width).powi(2))).sum();
            *s *= shape;
        }
        let mut sung = vec![0.0; count];
        fft.inverse(&spectrum, &mut sung);
        sung.iter().zip(&envelope).map(|(s, e)| s * e).collect()
    };

    let onsets: Vec<f64> = (0..notes.len()).map(|i| i as f64 / 3.0).collect();
    let melody = singer(&onsets, &vec![0.0; notes.len()], 5.5, &[(700.0, 80.0), (1200.0, 90.0), (2600.0, 120.0)]);
    let mut chords = vec![0.0; count];
    for step in [0.0, 4.0, 7.0] {
        let mut phase = 0.0;
        for i in 0..count {
            let root = [0.0, 7.0, 9.0, 5.0][((t[i] / 2.0).floor() as usize) % 4];
            phase += 2.0 * std::f64::consts::PI * 130.8 * 2f64.powf((root + step) / 12.0) / R as f64;
            chords[i] += (1..8).map(|k| (k as f64 * phase).sin() / k as f64).sum::<f64>();
        }
    }
    let noise = normal(&mut rng, 1.0, count);
    let drums: Vec<f64> = (0..count).map(|i| noise[i] * (-(t[i] % 0.5) * 20.0).exp()).collect();
    let mut left: Vec<f64> = (0..count).map(|i| melody[i] + 0.3 * chords[i] + 0.3 * drums[i]).collect();
    let mut right: Vec<f64> = (0..count).map(|i| 0.8 * melody[i] + 0.3 * chords[i]).collect();
    let rms = ((dot(&left, &left) + dot(&right, &right)) / (2 * count) as f64).sqrt();
    left.iter_mut().chain(right.iter_mut()).for_each(|x| *x *= 0.1 / rms);
    let response = jack_response(&mut rng);
    let leak: Vec<f64> =
        convolve(&left, &response[0]).iter().zip(convolve(&right, &response[1])).map(|(a, b)| a + b).collect();

    let timing = normal(&mut rng, 0.025, notes.len());
    let own_onsets: Vec<f64> = onsets.iter().zip(&timing).map(|(o, j)| o + j).collect();
    let cents = normal(&mut rng, 12.0, notes.len());
    let own = singer(&own_onsets, &cents, 5.1, &[(500.0, 70.0), (1500.0, 100.0), (2400.0, 110.0)]);
    let on = 15 * R..30 * R;
    let mut voice = vec![0.0; count];
    voice[on.clone()].copy_from_slice(&own[on.clone()]);
    // 6 dB above the leak.
    let gain = 2.0 * (dot(&leak[on.clone()], &leak[on.clone()]) / dot(&voice[on.clone()], &voice[on.clone()])).sqrt();
    voice.iter_mut().for_each(|v| *v *= gain);
    let talking = (0..count).map(|i| on.contains(&i)).collect();
    Leak { music: [left, right], leak, voice, talking }
}
