mod common;

use common::*;
use echobridge_dsp::FRAME;
use echobridge_dsp::leak::{LeakPath, Packet, Warning, align, analyze_leak, compare_leaks};
use rand::Rng;

/// Gently low-passed noise: more bass than treble, like typical music.
fn music(seconds: usize) -> Vec<f64> {
    let noise = normal(&mut rng(3), 0.1, R * seconds);
    // numpy.convolve(noise, ones(4) / 4, mode="same")
    (0..noise.len())
        .map(|i| (0..4).filter_map(|k| (i + 1).checked_sub(k).and_then(|j| noise.get(j))).sum::<f64>() / 4.0)
        .collect()
}

fn room(count: usize, seed: u64) -> Vec<f64> {
    normal(&mut rng(seed), 10f64.powf(-78.0 / 20.0), count)
}

fn delayed(signal: &[f64], delay: usize, gain_db: f64) -> Vec<f64> {
    let gain = 10f64.powf(gain_db / 20.0);
    let noise = room(signal.len(), 9);
    (0..signal.len()).map(|i| if i >= delay { signal[i - delay] * gain } else { 0.0 } + noise[i]).collect()
}

/// A shared ground couples a scaled, flat copy of the headphone signal.
fn electrical(reference: &[f64], gain_db: f64) -> Vec<f64> {
    delayed(reference, 96, gain_db)
}

/// Earcup leakage is strongly frequency dependent; a short resonant filter models it.
fn acoustic(reference: &[f64], gain_db: f64) -> Vec<f64> {
    let shaped = convolve(reference, &[1.0, -1.6, 0.8]);
    let std = |x: &[f64]| (dot(x, x) / x.len() as f64).sqrt();
    let scale = std(reference) / std(&shaped);
    let shaped: Vec<f64> = shaped.iter().map(|x| x * scale).collect();
    delayed(&shaped, 144, gain_db)
}

fn analyze(microphone: &[f64], reference: &[f64]) -> echobridge_dsp::leak::LeakMeasurement {
    analyze_leak(&to_f32(microphone), &to_f32(reference), 1.0).unwrap()
}

#[test]
fn flat_electrical_coupling_is_measured_with_its_delay() {
    let reference = music(6);
    let result = analyze(&electrical(&reference, -30.0), &reference);
    assert!(result.detected());
    assert!((result.leak_vs_playback_db + 30.0).abs() < 1.5, "{result:?}");
    assert!((result.delay_ms.unwrap() - 2.0).abs() < 0.1);
    assert!(result.gain_spread_db.unwrap() < 2.0);
    assert!(result.leak_share > 0.9);
    assert!(result.warnings().is_empty());
}

#[test]
fn frequency_shaped_acoustic_leak_is_not_flat() {
    let reference = music(6);
    let result = analyze(&acoustic(&reference, -24.0), &reference);
    assert!(result.detected());
    assert!(result.gain_spread_db.unwrap() > 8.0);
}

#[test]
fn covering_earcups_without_change_means_electrical() {
    let reference = music(6);
    let worn = analyze(&electrical(&reference, -30.0), &reference);
    let covered = analyze(&electrical(&reference, -31.0), &reference);
    let verdict = compare_leaks(&worn, &covered);
    assert_eq!(verdict.path, LeakPath::Electrical);
    assert!(verdict.detail.contains("flat across frequencies"));
}

#[test]
fn covering_earcups_that_removes_the_leak_means_acoustic() {
    let reference = music(6);
    let worn = analyze(&acoustic(&reference, -24.0), &reference);
    let covered = analyze(&acoustic(&reference, -50.0), &reference);
    assert_eq!(compare_leaks(&worn, &covered).path, LeakPath::Acoustic);
    let silent = analyze(&room(reference.len(), 9), &reference);
    assert!(!silent.detected());
    assert_eq!(compare_leaks(&worn, &silent).path, LeakPath::Acoustic);
}

#[test]
fn partial_reduction_is_reported_as_mixed() {
    let reference = music(6);
    let worn = analyze(&electrical(&reference, -30.0), &reference);
    let covered = analyze(&electrical(&reference, -38.0), &reference);
    assert_eq!(compare_leaks(&worn, &covered).path, LeakPath::Mixed);
}

#[test]
fn missing_playback_and_speech_produce_warnings() {
    let quiet_reference: Vec<f64> = music(6).iter().map(|x| x * 1e-4).collect();
    let quiet = analyze(&room(quiet_reference.len(), 9), &quiet_reference);
    assert!(!quiet.detected());
    assert_eq!(compare_leaks(&quiet, &quiet).path, LeakPath::None);
    assert_eq!(quiet.warnings()[0], Warning::NoPlayback);

    let reference = music(6);
    let speech = normal(&mut rng(4), 0.05, reference.len());
    let talking: Vec<f64> = electrical(&reference, -30.0).iter().zip(&speech).map(|(a, b)| a + b).collect();
    let result = analyze_leak(&to_f32(&talking), &to_f32(&reference), 0.5).unwrap();
    assert!(result.warnings().contains(&Warning::Covered));
    assert!(result.warnings().contains(&Warning::PlaybackGaps));
}

#[test]
fn short_recordings_are_rejected() {
    assert!(analyze_leak(&[0.0; 1000], &[0.0; 1000], 1.0).is_err());
}

/// Packets as WASAPI delivers them: jittery stamps, different block sizes per stream, and
/// no loopback packets while nothing plays (`silent` seconds).
fn record(seconds: usize, silent: Option<(f64, f64)>) -> (Vec<Packet>, Vec<Packet>) {
    let mut rng = rng(21);
    let reference = normal(&mut rng, 0.1, R * seconds);
    let delay = R * 3 / 1000;
    let leak: Vec<f64> = (0..reference.len())
        .map(|i| if i >= delay { reference[i - delay] * 10f64.powf(-30.0 / 20.0) } else { 0.0 })
        .collect();
    let mut packets = |signal: &[f64], size: usize, skip: Option<(f64, f64)>| {
        let mut result = Vec::new();
        for start in (0..signal.len()).step_by(size) {
            let time = start as f64 / R as f64;
            if skip.is_some_and(|(from, to)| from <= time && time < to) {
                continue;
            }
            let end = (start + size).min(signal.len());
            let jitter = rng.gen_range(-0.0015..0.0015);
            result.push(Packet { timestamp: 100.0 + time + jitter, samples: to_f32(&signal[start..end]) });
        }
        result
    };
    let playback = packets(&reference, 448, silent);
    let microphone = packets(&leak, FRAME, None);
    (microphone, playback)
}

#[test]
fn aligned_recording_preserves_a_known_leak() {
    let (microphone, playback) = record(4, None);
    let recording = align(&microphone, &playback);
    assert!(recording.coverage > 0.99);
    let result = analyze_leak(&recording.microphone, &recording.reference, recording.coverage).unwrap();
    assert!((result.leak_vs_playback_db + 30.0).abs() < 1.0, "{result:?}");
    assert!((result.delay_ms.unwrap() - 3.0).abs() < 0.2);
    assert!(result.bands.iter().all(|b| b.coherence > 0.95));
    assert!(result.gain_spread_db.unwrap() < 2.0);
}

#[test]
fn playback_silence_keeps_alignment_after_it_resumes() {
    let (microphone, playback) = record(6, Some((2.0, 3.0)));
    let recording = align(&microphone, &playback);
    assert!((recording.coverage - 5.0 / 6.0).abs() < 0.02);
    let resumed = 3 * R + 4800..recording.microphone.len();
    let result = analyze_leak(&recording.microphone[resumed.clone()], &recording.reference[resumed], 1.0).unwrap();
    assert!((result.leak_vs_playback_db + 30.0).abs() < 1.0);
    assert!((result.delay_ms.unwrap() - 3.0).abs() < 0.2);
}
