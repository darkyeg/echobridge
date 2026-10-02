mod common;

use common::*;
use echobridge_dsp::linear::{LinearCanceller, LinearConfig};

fn cancel(signal: &Leak, near: &[f64]) -> Vec<f64> {
    let mut canceller = LinearCanceller::new(LinearConfig::default());
    let far = interleave(&signal.music[0], &signal.music[1]);
    let mut out = vec![0.0f32; near.len()];
    canceller.process(&to_f32(near), &far, &mut out).unwrap();
    out.iter().map(|&s| f64::from(s)).collect()
}

fn select(x: &[f64], mask: &[bool]) -> Vec<f64> {
    x.iter().zip(mask).filter(|(_, m)| **m).map(|(v, _)| *v).collect()
}

#[test]
fn removes_a_strong_stereo_leak_after_a_few_seconds() {
    let signal = jack_leak(12, None);
    let output = cancel(&signal, &signal.leak);
    // Correlated stereo playback converges gradually; by 8 s it is far below hearing.
    let settled = 8 * R..;
    let removed = db(&signal.leak[settled.clone()]) - db(&output[settled]);
    assert!(removed > 40.0, "removed {removed:.1} dB");
}

#[test]
fn speech_passes_unchanged_while_the_leak_is_removed() {
    let signal = jack_leak(28, Some(10));
    let near: Vec<f64> = signal.leak.iter().zip(&signal.voice).map(|(l, v)| l + v).collect();
    let output = cancel(&signal, &near);
    // The canceller is linear, so whatever is not the voice is leftover leak.
    let leftover: Vec<f64> = output.iter().zip(&signal.voice).map(|(o, v)| o - v).collect();
    let voice = select(&signal.voice, &signal.talking);
    let leftover = select(&leftover, &signal.talking);
    let leak = select(&signal.leak, &signal.talking);
    assert!(db(&voice) - db(&leftover) > 30.0, "voice to leftover {:.1} dB", db(&voice) - db(&leftover));
    assert!(db(&leak) - db(&leftover) > 25.0, "removed {:.1} dB", db(&leak) - db(&leftover));
}

#[test]
fn singing_along_keeps_the_voice_and_the_song_removed() {
    let signal = sing_along(40);
    let near: Vec<f64> = signal.leak.iter().zip(&signal.voice).map(|(l, v)| l + v).collect();
    let output = cancel(&signal, &near);
    // Every 200 ms of singing keeps its level: no part of the voice is cancelled.
    let window = R / 5;
    for start in (15 * R..30 * R).step_by(window) {
        let part = start..start + window;
        let voice = &signal.voice[part.clone()];
        let kept = dot(&output[part.clone()], voice) / dot(voice, voice);
        assert!(10.0 * kept.log10() > -1.5, "voice cut at {:.1} s", start as f64 / R as f64);
    }
    let leftover: Vec<f64> = output.iter().zip(&signal.voice).map(|(o, v)| o - v).collect();
    for (label, part) in [("while singing", 15 * R..30 * R), ("after singing", 31 * R..40 * R)] {
        let removed = db(&signal.leak[part.clone()]) - db(&leftover[part]);
        assert!(removed > 20.0, "{label}: removed {removed:.1} dB");
    }
}

/// Writes the sing-along input and this canceller's output for comparison with the Python
/// prototype: `ECHOBRIDGE_PARITY_DIR=<folder> cargo test -p echobridge-dsp -- --ignored`.
#[test]
#[ignore]
fn export_for_parity() {
    let Ok(folder) = std::env::var("ECHOBRIDGE_PARITY_DIR") else { return };
    let signal = sing_along(40);
    let near: Vec<f64> = signal.leak.iter().zip(&signal.voice).map(|(l, v)| l + v).collect();
    let output = cancel(&signal, &near);
    let write = |name: &str, data: &[f32]| {
        let bytes: Vec<u8> = data.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(std::path::Path::new(&folder).join(name), bytes).unwrap();
    };
    write("near.f32", &to_f32(&near));
    write("far.f32", &interleave(&signal.music[0], &signal.music[1]));
    write("voice.f32", &to_f32(&signal.voice));
    write("leak.f32", &to_f32(&signal.leak));
    write("rust.f32", &to_f32(&output));
}
