//! How much playback leaks into the microphone, and by which path.
//!
//! A recording of the microphone while the user is silent and playback runs is compared
//! with the playback itself. The coherent part of the microphone is the leak a linear
//! filter could remove; its gain per frequency tells an electrical leak (flat, through a
//! shared ground) from an acoustic one (shaped by the earcups and the air).

use realfft::num_complex::Complex64;
use serde::Serialize;

use crate::clock::BlockClock;
use crate::fft::RealFft;
use crate::level::power_db;
use crate::{DspError, RATE};

pub const BANDS: [(u32, u32); 6] = [(50, 150), (150, 400), (400, 1000), (1000, 2500), (2500, 6000), (6000, 12000)];
const SEGMENT: usize = 8192;
/// Below this playback level the recording cannot show a leak.
const QUIET_PLAYBACK_DBFS: f64 = -55.0;
/// Band coherence above this means the microphone carries a copy of the playback.
const DETECTED_COHERENCE: f64 = 0.3;
const RELIABLE_COHERENCE: f64 = 0.5;
/// Covering the earcups blocks the acoustic path by roughly 15-30 dB; wiring is unaffected.
const ACOUSTIC_DROP_DB: f64 = 12.0;
const ELECTRICAL_DROP_DB: f64 = 4.0;
const FLAT_SPREAD_DB: f64 = 4.0;
/// The leak is searched for within this delay of the playback, in seconds.
const DELAY_LIMIT: f64 = 0.3;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Band {
    pub low_hz: u32,
    pub high_hz: u32,
    pub leak_gain_db: f64,
    pub coherence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeakMeasurement {
    pub seconds: f64,
    pub playback_dbfs: f64,
    pub microphone_dbfs: f64,
    pub leak_dbfs: f64,
    pub leak_vs_playback_db: f64,
    /// Share of the microphone's energy that is a linear copy of the playback.
    pub leak_share: f64,
    pub delay_ms: Option<f64>,
    /// Gain difference across reliable bands; a small spread means a flat, electrical leak.
    pub gain_spread_db: Option<f64>,
    pub reference_coverage: f64,
    pub bands: Vec<Band>,
}

/// Something that makes a measurement unreliable, in words for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Warning {
    NoPlayback,
    PlaybackGaps,
    Covered,
}

impl Warning {
    pub fn message(self) -> &'static str {
        match self {
            Self::NoPlayback => "No playback was detected. Play music on the selected headphones.",
            Self::PlaybackGaps => "Playback audio was missing for part of the recording.",
            Self::Covered => "Speech or room noise covered the leak. Stay silent while recording.",
        }
    }
}

impl LeakMeasurement {
    pub fn detected(&self) -> bool {
        self.bands.iter().any(|b| b.coherence >= DETECTED_COHERENCE)
    }

    pub fn warnings(&self) -> Vec<Warning> {
        let mut problems = Vec::new();
        if self.playback_dbfs < QUIET_PLAYBACK_DBFS {
            problems.push(Warning::NoPlayback);
        }
        if self.reference_coverage < 0.9 {
            problems.push(Warning::PlaybackGaps);
        }
        if self.microphone_dbfs > -50.0 && self.leak_share < 0.3 {
            problems.push(Warning::Covered);
        }
        problems
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LeakPath {
    None,
    Acoustic,
    Electrical,
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub path: LeakPath,
    pub title: &'static str,
    pub detail: String,
    /// How much covering the earcups lowered the leak, in dB.
    pub drop_db: Option<f64>,
}

/// Measure the leak of mono `reference` playback in mono `microphone`, recorded together
/// at [`RATE`] and roughly aligned. `coverage` is the share of the recording with playback.
pub fn analyze_leak(microphone: &[f32], reference: &[f32], coverage: f64) -> Result<LeakMeasurement, DspError> {
    let count = microphone.len().min(reference.len());
    if count < 2 * SEGMENT {
        return Err(DspError::TooShort { seconds: 1 });
    }
    let microphone: Vec<f64> = microphone[..count].iter().map(|&s| f64::from(s)).collect();
    let reference: Vec<f64> = reference[..count].iter().map(|&s| f64::from(s)).collect();
    let rate = f64::from(RATE);
    let lag = delay(&microphone, &reference, (DELAY_LIMIT * rate) as usize);
    let aligned = shift(&reference, lag);

    let mut fft = RealFft::new(SEGMENT);
    let bins = fft.bins();
    let window = hanning(SEGMENT);
    let (mut mic_power, mut ref_power) = (vec![0.0; bins], vec![0.0; bins]);
    let mut cross = vec![Complex64::default(); bins];
    let (mut mic_spectrum, mut ref_spectrum) = (cross.clone(), cross.clone());
    let mut segment = vec![0.0; SEGMENT];
    for start in (0..=count - SEGMENT).step_by(SEGMENT / 2) {
        for (source, spectrum) in [(&microphone, &mut mic_spectrum), (&aligned, &mut ref_spectrum)] {
            for ((s, x), w) in segment.iter_mut().zip(&source[start..start + SEGMENT]).zip(&window) {
                *s = x * w;
            }
            fft.forward(&segment, spectrum);
        }
        for k in 0..bins {
            mic_power[k] += mic_spectrum[k].norm_sqr();
            ref_power[k] += ref_spectrum[k].norm_sqr();
            cross[k] += mic_spectrum[k] * ref_spectrum[k].conj();
        }
    }
    let coherence: Vec<f64> = (0..bins).map(|k| cross[k].norm_sqr() / (mic_power[k] * ref_power[k] + 1e-30)).collect();
    let gain: Vec<f64> = (0..bins).map(|k| cross[k].norm() / (ref_power[k] + 1e-30)).collect();

    // An ideal linear filter could remove exactly the coherent part of the microphone energy.
    let total: f64 = mic_power.iter().sum();
    let share = coherence.iter().zip(&mic_power).map(|(c, p)| c * p).sum::<f64>() / total.max(1e-30);
    let mean_square = |x: &[f64]| x.iter().map(|s| s * s).sum::<f64>() / x.len() as f64;
    let microphone_dbfs = power_db(mean_square(&microphone));
    let playback_dbfs = power_db(mean_square(&reference));
    let leak_dbfs = microphone_dbfs + power_db(share);
    let bands: Vec<Band> = BANDS
        .iter()
        .map(|&(low, high)| {
            let selected = |k: &usize| {
                let frequency = *k as f64 * rate / SEGMENT as f64;
                frequency >= f64::from(low) && frequency < f64::from(high)
            };
            let gains: Vec<f64> = (0..bins).filter(selected).map(|k| gain[k]).collect();
            let coherences: Vec<f64> = (0..bins).filter(selected).map(|k| coherence[k]).collect();
            Band {
                low_hz: low,
                high_hz: high,
                leak_gain_db: 20.0 * median(gains).max(1e-9).log10(),
                coherence: median(coherences),
            }
        })
        .collect();
    let reliable: Vec<f64> =
        bands.iter().filter(|b| b.coherence >= RELIABLE_COHERENCE).map(|b| b.leak_gain_db).collect();
    let gain_spread_db = (reliable.len() >= 3)
        .then(|| reliable.iter().copied().fold(f64::MIN, f64::max) - reliable.iter().copied().fold(f64::MAX, f64::min));
    let detected = bands.iter().any(|b| b.coherence >= DETECTED_COHERENCE);
    Ok(LeakMeasurement {
        seconds: count as f64 / rate,
        playback_dbfs,
        microphone_dbfs,
        leak_dbfs,
        leak_vs_playback_db: leak_dbfs - playback_dbfs,
        leak_share: share,
        delay_ms: detected.then(|| lag as f64 / rate * 1000.0),
        gain_spread_db,
        reference_coverage: coverage,
        bands,
    })
}

/// Classify the leak path from a normal recording and one with the earcups covered.
pub fn compare_leaks(worn: &LeakMeasurement, covered: &LeakMeasurement) -> Verdict {
    if !worn.detected() {
        return Verdict {
            path: LeakPath::None,
            title: "No measurable leak",
            detail: "The microphone did not pick up a copy of the playback with the headset on.".into(),
            drop_db: None,
        };
    }
    let mut drop = worn.leak_vs_playback_db - covered.leak_vs_playback_db;
    if !covered.detected() {
        drop = drop.max(ACOUSTIC_DROP_DB);
    }
    if drop >= ACOUSTIC_DROP_DB {
        return Verdict {
            path: LeakPath::Acoustic,
            title: "Sound is escaping from the earcups",
            detail: "Covering the earcups removed most of the leak, so it travels through the air. \
                     Lower the headphone volume, use closed-back earcups, or move the microphone away."
                .into(),
            drop_db: Some(drop),
        };
    }
    if drop <= ELECTRICAL_DROP_DB {
        let flat = worn.gain_spread_db.is_some_and(|spread| spread <= FLAT_SPREAD_DB);
        return Verdict {
            path: LeakPath::Electrical,
            title: "The leak is electrical, not acoustic",
            detail: format!(
                "Covering the earcups did not reduce the leak{}. Headphone current is coupling into \
                 the microphone, typically through a shared ground in a combined headset jack. \
                 Choose Clean voice, which removes such a leak without altering your voice. A USB \
                 audio adapter or USB headset gives the microphone a separate ground.",
                if flat { " and it is flat across frequencies" } else { "" }
            ),
            drop_db: Some(drop),
        };
    }
    Verdict {
        path: LeakPath::Mixed,
        title: "Both paths contribute",
        detail: "Covering the earcups reduced the leak only partly. Part of it travels through the air \
                 and part of it through the wiring."
            .into(),
        drop_db: Some(drop),
    }
}

/// A block of a recording: the device time of its first sample and mono samples.
#[derive(Debug, Clone)]
pub struct Packet {
    pub timestamp: f64,
    pub samples: Vec<f32>,
}

/// A microphone and playback recording on one sample grid.
#[derive(Debug, Clone)]
pub struct AlignedRecording {
    pub microphone: Vec<f32>,
    pub reference: Vec<f32>,
    /// Share of the recording that has playback.
    pub coverage: f64,
}

/// Place recorded microphone and playback packets on one sample grid at [`RATE`].
pub fn align(microphone: &[Packet], reference: &[Packet]) -> AlignedRecording {
    let mic = place_on_grid(microphone);
    let playback = place_on_grid(reference);
    let Some((origin, last)) = mic.first().zip(mic.last()) else {
        return AlignedRecording { microphone: Vec::new(), reference: Vec::new(), coverage: 0.0 };
    };
    let origin = origin.0;
    let index_of = |start: f64| ((start - origin) * f64::from(RATE)).round() as i64;
    let total = (index_of(last.0) + last.1.len() as i64).max(0) as usize;
    let mut aligned = AlignedRecording { microphone: vec![0.0; total], reference: vec![0.0; total], coverage: 0.0 };
    for (start, samples) in &mic {
        let index = index_of(*start).max(0) as usize;
        let end = (index + samples.len()).min(total);
        aligned.microphone[index..end].copy_from_slice(&samples[..end - index]);
    }
    let mut covered = vec![false; total];
    for (start, samples) in &playback {
        let index = index_of(*start);
        let low = index.max(0);
        let high = (index + samples.len() as i64).min(total as i64);
        for i in low..high {
            aligned.reference[i as usize] = samples[(i - index) as usize];
            covered[i as usize] = true;
        }
    }
    aligned.coverage = if total == 0 { 0.0 } else { covered.iter().filter(|&&c| c).count() as f64 / total as f64 };
    aligned
}

/// Contiguous placement, with each contiguous segment anchored at its median stamp error:
/// a whole segment anchors far better than its first jittery stamp.
fn place_on_grid(packets: &[Packet]) -> Vec<(f64, &[f32])> {
    let mut clock = BlockClock::contiguous(RATE);
    let placed: Vec<_> = packets.iter().map(|p| (clock.place(p.timestamp, p.samples.len()), p)).collect();
    let mut result = Vec::with_capacity(placed.len());
    let mut first = 0;
    for end in 1..=placed.len() {
        if end < placed.len() && !placed[end].0.restarted {
            continue;
        }
        let segment = &placed[first..end];
        let shift = median(segment.iter().map(|(p, packet)| packet.timestamp - p.start).collect());
        result.extend(segment.iter().map(|(p, packet)| (p.start + shift, packet.samples.as_slice())));
        first = end;
    }
    result
}

/// The lag (samples) that best aligns `reference` to `microphone`, within `limit`, by
/// phase-transform cross-correlation.
fn delay(microphone: &[f64], reference: &[f64], limit: usize) -> i64 {
    let size = (2 * microphone.len()).next_power_of_two();
    let mut fft = RealFft::new(size);
    let mut a = vec![Complex64::default(); fft.bins()];
    let mut b = a.clone();
    fft.forward(microphone, &mut a);
    fft.forward(reference, &mut b);
    for (x, y) in a.iter_mut().zip(&b) {
        let cross = *x * y.conj();
        *x = cross / (cross.norm() + 1e-12);
    }
    let mut correlation = vec![0.0; size];
    fft.inverse(&a, &mut correlation);
    let lags =
        (-(limit as i64)..=limit as i64).map(|lag| (lag, correlation[lag.rem_euclid(size as i64) as usize].abs()));
    lags.fold((0, f64::MIN), |best, item| if item.1 > best.1 { item } else { best }).0
}

fn shift(samples: &[f64], lag: i64) -> Vec<f64> {
    let n = samples.len();
    let mut shifted = vec![0.0; n];
    let lag_size = lag.unsigned_abs() as usize;
    if lag_size < n {
        if lag >= 0 {
            shifted[lag_size..].copy_from_slice(&samples[..n - lag_size]);
        } else {
            shifted[..n - lag_size].copy_from_slice(&samples[lag_size..]);
        }
    }
    shifted
}

/// `numpy.hanning`.
fn hanning(size: usize) -> Vec<f64> {
    let denominator = (size - 1) as f64;
    (0..size).map(|n| 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / denominator).cos()).collect()
}

/// `numpy.median`; zero for no values.
fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 1 { values[middle] } else { (values[middle - 1] + values[middle]) / 2.0 }
}
