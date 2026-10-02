//! The guided leak test: record with the headset worn, then with the earcups covered, and
//! compare. Audio stays in memory; reports hold numbers only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

use echobridge_audio::{AudioBackend, DeviceId};
use echobridge_dsp::leak::{LeakMeasurement, Verdict, analyze_leak};
use echobridge_engine::{RecordError, record_leak};
use serde::Serialize;

/// Length of each step's recording.
pub const STEP_SECONDS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Worn,
    Covered,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// Share of the recording done, from 0 to 1.
    Recording(f32),
    Measured(LeakMeasurement),
    Failed(String),
    Cancelled,
}

/// One step being recorded and analyzed on its own thread.
#[derive(Debug)]
pub struct Recording {
    pub step: Step,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<Progress>>,
    thread: Option<JoinHandle<()>>,
}

impl Recording {
    pub fn start(
        backend: Arc<dyn AudioBackend>,
        microphone: DeviceId,
        playback: DeviceId,
        step: Step,
        duration: Duration,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(Progress::Recording(0.0)));
        let (stop, shared) = (cancel.clone(), progress.clone());
        let thread = std::thread::Builder::new()
            .name("EchoBridge leak test".into())
            .spawn(move || {
                let mut report = |fraction| *shared.lock().unwrap() = Progress::Recording(fraction);
                let result = record_leak(backend.as_ref(), &microphone, &playback, duration, &stop, &mut report);
                let outcome = match result {
                    Ok(recording) => {
                        match analyze_leak(&recording.microphone, &recording.reference, recording.coverage) {
                            Ok(measurement) => Progress::Measured(measurement),
                            Err(error) => Progress::Failed(error.to_string()),
                        }
                    }
                    Err(RecordError::Cancelled) => Progress::Cancelled,
                    Err(error) => Progress::Failed(error.to_string()),
                };
                *shared.lock().unwrap() = outcome;
            })
            .expect("the leak test thread starts");
        Self { step, cancel, progress, thread: Some(thread) }
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap().clone()
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// A step's result in words.
pub fn describe(measurement: &LeakMeasurement) -> String {
    let mut lines = vec![if measurement.detected() {
        let mut text = format!(
            "Leak {:.1} dBFS, {:+.1} dB relative to playback, {:.0}% of the microphone signal.",
            measurement.leak_dbfs,
            measurement.leak_vs_playback_db,
            measurement.leak_share * 100.0
        );
        if let Some(spread) = measurement.gain_spread_db {
            text.push_str(&format!(" Spread across frequencies: {spread:.1} dB."));
        }
        text
    } else {
        format!("No copy of the playback found (microphone {:.1} dBFS).", measurement.microphone_dbfs)
    }];
    lines.extend(measurement.warnings().into_iter().map(|w| w.message().to_string()));
    lines.join("\n")
}

/// The verdict's explanation, with how much covering changed the leak.
pub fn verdict_detail(verdict: &Verdict) -> String {
    match verdict.drop_db {
        Some(drop) => format!("{}\n\nCovering the earcups changed the leak by {drop:.1} dB.", verdict.detail),
        None => verdict.detail.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Devices {
    pub microphone: String,
    pub playback: String,
}

#[derive(Debug, Serialize)]
pub struct Report<'a> {
    pub kind: &'static str,
    pub created: String,
    pub seconds_per_step: u64,
    pub devices: &'a Devices,
    pub worn: &'a LeakMeasurement,
    pub covered: &'a LeakMeasurement,
    pub verdict: &'a Verdict,
    pub audio_saved: bool,
}

impl<'a> Report<'a> {
    pub fn new(
        devices: &'a Devices,
        worn: &'a LeakMeasurement,
        covered: &'a LeakMeasurement,
        verdict: &'a Verdict,
    ) -> Self {
        Self {
            kind: "leak-test",
            created: Timestamp::now().iso(),
            seconds_per_step: STEP_SECONDS,
            devices,
            worn,
            covered,
            verdict,
            audio_saved: false,
        }
    }

    /// Write the report into `folder` and return its path.
    pub fn save(&self, folder: &Path) -> io::Result<PathBuf> {
        fs::create_dir_all(folder)?;
        let path = folder.join(format!("leak-test-{}.json", Timestamp::now().compact()));
        fs::write(&path, serde_json::to_string_pretty(self).map_err(io::Error::other)?)?;
        Ok(path)
    }
}

/// A UTC time of day, without a date library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timestamp {
    year: i64,
    month: u32,
    day: u32,
    seconds_of_day: u64,
}

impl Timestamp {
    pub(crate) fn now() -> Self {
        Self::from_unix(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
    }

    /// Civil date from days since 1970 (Howard Hinnant's algorithm).
    fn from_unix(seconds: u64) -> Self {
        let days = (seconds / 86_400) as i64 + 719_468;
        let era = days.div_euclid(146_097);
        let day_of_era = days.rem_euclid(146_097);
        let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
        let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u32;
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        Self { year, month, day, seconds_of_day: seconds % 86_400 }
    }

    fn clock(self) -> (u64, u64, u64) {
        (self.seconds_of_day / 3600, self.seconds_of_day / 60 % 60, self.seconds_of_day % 60)
    }

    pub(crate) fn iso(self) -> String {
        let (h, m, s) = self.clock();
        format!("{:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z", self.year, self.month, self.day)
    }

    fn compact(self) -> String {
        let (h, m, s) = self.clock();
        format!("{:04}{:02}{:02}-{h:02}{m:02}{s:02}", self.year, self.month, self.day)
    }
}

#[cfg(test)]
mod tests {
    use echobridge_audio::fake::{FakeBackend, HEADPHONES, MICROPHONE, Script};
    use echobridge_dsp::leak::compare_leaks;

    use super::*;

    #[test]
    fn timestamps_are_utc_dates() {
        assert_eq!(Timestamp::from_unix(0).iso(), "1970-01-01T00:00:00Z");
        assert_eq!(Timestamp::from_unix(951_782_400).iso(), "2000-02-29T00:00:00Z");
        assert_eq!(Timestamp::from_unix(1_790_946_062).compact(), "20261002-130102");
    }

    /// A wired leak: the microphone carries a delayed, scaled copy of the playback.
    fn leaking_script(seconds: usize) -> Script {
        let mut state = 5u32;
        let total = seconds * 48_000;
        let playback: Vec<f32> = (0..2 * total)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                0.2 * ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
            })
            .collect();
        let microphone = (0..total).map(|t| if t >= 45 { 0.3 * playback[2 * (t - 45)] } else { 0.0 }).collect();
        Script { microphone, playback, playback_gain: 1.0, tick: Duration::from_millis(2), output_failure_after: None }
    }

    fn measure(step: Step) -> LeakMeasurement {
        let backend = Arc::new(FakeBackend::new(leaking_script(4)));
        let recording =
            Recording::start(backend, MICROPHONE.into(), HEADPHONES.into(), step, Duration::from_millis(1500));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            match recording.progress() {
                Progress::Measured(measurement) => return measurement,
                Progress::Recording(_) => assert!(std::time::Instant::now() < deadline, "the recording never ended"),
                other => panic!("{other:?}"),
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn records_measures_and_reports_a_wired_leak() {
        let worn = measure(Step::Worn);
        assert!(worn.detected(), "{worn:?}");
        assert!(describe(&worn).starts_with("Leak "));
        let covered = measure(Step::Covered);
        let verdict = compare_leaks(&worn, &covered);
        assert_eq!(verdict.path, echobridge_dsp::leak::LeakPath::Electrical);

        let devices = Devices { microphone: "Mic".into(), playback: "Headphones".into() };
        let folder = std::env::temp_dir().join(format!("echobridge-reports-{}", std::process::id()));
        let path = Report::new(&devices, &worn, &covered, &verdict).save(&folder).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["verdict"]["path"], "electrical");
        assert_eq!(saved["audio_saved"], false);
        fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn dropping_a_recording_cancels_it() {
        let backend = Arc::new(FakeBackend::new(leaking_script(30)));
        let started = std::time::Instant::now();
        drop(Recording::start(backend, MICROPHONE.into(), HEADPHONES.into(), Step::Worn, Duration::from_secs(20)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
