//! Adaptive echo removal's second path, and the guard that decides when the call may hear it.
//!
//! Adaptive runs two AEC3 cores on the same audio. The Strong core always runs: it is what the
//! call hears while the echo path is being learned and whenever echo might reach the high
//! frequencies. The second core keeps the voice's highs clear. [`ReleaseGuard`] lets the call
//! hear it only after measurements show the playback leak has no high-frequency content, and
//! takes it away again as soon as one of them turns unsafe.
//!
//! Every threshold below is a heuristic, not a proof. The guard cannot tell speech highs from
//! echo highs, so it prefers Strong whenever a measurement is ambiguous or inconclusive.
//!
//! The playback is judged per channel, never only as a mono mix: channels that are out of phase
//! cancel in a mix, yet each can leak into the microphone on its own.

use echobridge_aec3::{Config, EchoProcessor, Error, Metrics};
use echobridge_dsp::level::{FLOOR_DBFS, dbfs, power_db};
use echobridge_dsp::{FRAME, RATE};

/// Frames between reads of AEC3's statistics.
const METRICS_INTERVAL: u64 = 10;
/// Frames in 10 ms, for converting AEC3's delay estimate.
const FRAME_MS: i32 = 10;
/// The longest echo delay AEC3 accepts, in frames.
const MAX_DELAY_FRAMES: usize = 25;
/// Playback history: the longest delay, plus a frame either side for estimation error.
const HISTORY_FRAMES: usize = MAX_DELAY_FRAMES + 2;
/// Delays compared: the estimate and the frame either side.
const LAGS: usize = 3;
/// Playback highs measured: the left channel, the right channel, and their mean power.
const SERIES: usize = 3;
/// Correlations computed per frame of evidence: every playback series at every delay.
const PAIRS: usize = SERIES * LAGS;
/// Frames with voice-free echo evidence needed before the highs are released.
const WINDOW_FRAMES: usize = 300;
/// Frames of playback, voiced or not, over which the microphone's highs are checked for echo:
/// one second. Shorter than [`WINDOW_FRAMES`] so a leak that appears under a voice is caught
/// within about a second. Not shorter still: music and speech envelopes change slowly, so a
/// window holds few independent samples and its correlation wanders far from zero by chance.
const WATCH_FRAMES: usize = 100;
/// Frames after a fallback, restart or reset before the highs may be released again.
const DWELL_FRAMES: u32 = 500;

/// The high-pass corner that separates the highs Strong muffles from the rest, in Hz.
const HIGHS_CUTOFF_HZ: f64 = 4_000.0;
/// Playback highs below this level cannot show whether the leak has highs.
const PLAYBACK_HIGHS_FLOOR_DBFS: f32 = -55.0;
/// Playback this loud should be cancelled; weak cancellation then matters.
const PLAYBACK_ACTIVE_DBFS: f32 = -55.0;
/// Strong's output this far under the microphone means echo, not a voice, dominates it.
const VOICE_FREE_MARGIN_DB: f32 = 12.0;
/// Playback highs must vary this much (standard deviation, dB) to be correlated at all.
const MIN_PLAYBACK_STD_DB: f64 = 3.0;
/// Highs that vary less than this (variance, dB squared) carry no usable envelope.
const MIN_OBSERVED_VARIANCE: f64 = 1e-4;

/// AEC3 has learned the echo path when either figure holds for [`LEARNED_FRAMES`].
///
/// The pinned AEC3 cannot report more than 6.0 dB ERLE (`erle.max_l` of 4, so 10 log10 4) or
/// 20 dB ERL (an ERL floor of 0.01), so the thresholds sit just under those caps.
const LEARNED_ERLE_DB: f64 = 5.5;
const LEARNED_ERL_DB: f64 = 19.0;
const LEARNED_FRAMES: u32 = 100;

/// Release needs envelope correlation at most this, or the stricter value with AI noise
/// removal, which alters the highs the correlation reads.
const RELEASE_CORRELATION: f64 = 0.25;
const RELEASE_CORRELATION_AI: f64 = 0.15;
/// Correlation this high over [`WINDOW_FRAMES`] voice-free frames while released means the
/// highs carry echo.
const FALLBACK_CORRELATION: f64 = 0.45;
/// The same over [`WATCH_FRAMES`] frames of any kind. On synthetic speech over a leak without
/// highs, such a correlation reached 0.45 on 1.4% of frames and 0.7 once in 48,000, so 0.45
/// would drop the highs within seconds of a voice. A leak with highs read above 0.8 on 99%
/// of frames, within a second of appearing.
const WATCH_CORRELATION: f64 = 0.8;
/// Released highs must stay this far under the microphone's highs to release...
const RELEASE_MARGIN_DB: f32 = 20.0;
/// ...and over this margin for [`FALLBACK_FRAMES`] voice-free frames in a row means they carry
/// too much. Under a voice the released highs are the voice's, restored, and are meant to be
/// large next to the microphone's, so only the correlation with the playback judges them there.
const FALLBACK_MARGIN_DB: f32 = 12.0;
const FALLBACK_FRAMES: u32 = 5;
/// Released highs below this level are inaudible whatever the microphone's highs are.
const AUDIBLE_RELEASED_DBFS: f32 = -60.0;
/// Cancellation is failing when ERLE and ERL are both under these while playback is active.
/// Either figure alone can show learning, so either alone being low is not a failure.
const FALLBACK_ERLE_DB: f64 = 5.0;
const FALLBACK_ERL_DB: f64 = 17.0;

/// Samples a crossfade towards the released path takes: 300 ms.
const RELEASE_RAMP: usize = RATE as usize * 3 / 10;
/// Samples a crossfade back to Strong takes: 80 ms.
const FALLBACK_RAMP: usize = RATE as usize * 2 / 25;
/// A missing reference or a restart reaches Strong within one frame.
const IMMEDIATE_RAMP: usize = FRAME;

/// What one 10 ms frame says about the echo path, in dBFS.
#[derive(Debug, Clone, Copy)]
struct FrameLevels {
    /// Every sample of the playback reference was present.
    complete: bool,
    microphone: f32,
    /// Both playback channels together.
    playback: f32,
    /// The Strong core's output.
    strong: f32,
    /// The second core's output, which keeps a voice that Strong suppressed.
    released: f32,
    microphone_highs: f32,
    playback_highs: [f32; SERIES],
    /// The highs the second core adds to Strong's output.
    released_highs: f32,
}

impl FrameLevels {
    /// Both cores removed most of the microphone, so echo rather than a voice dominates it.
    /// Strong alone would call a quiet voice echo, having suppressed it; the second core
    /// is tuned to keep a voice, so it does not agree.
    fn is_voice_free(&self) -> bool {
        let limit = self.microphone - VOICE_FREE_MARGIN_DB;
        self.strong <= limit && self.released <= limit
    }

    /// Frames that can show whether the leak has highs: complete, audible highs, no voice.
    fn is_evidence(&self) -> bool {
        self.complete && loudest(&self.playback_highs) > PLAYBACK_HIGHS_FLOOR_DBFS && self.is_voice_free()
    }

    /// The released highs are large next to what the microphone holds in the highs.
    fn releases_too_much(&self) -> bool {
        self.released_highs > AUDIBLE_RELEASED_DBFS && self.released_highs > self.microphone_highs - FALLBACK_MARGIN_DB
    }
}

fn loudest(levels: &[f32; SERIES]) -> f32 {
    levels.iter().copied().fold(FLOOR_DBFS, f32::max)
}

/// How far the call hears the released path, and how fast it gets there.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Verdict {
    /// 0.0 is Strong's output alone, 1.0 the released path's.
    target: f32,
    /// Samples a full crossfade to `target` takes.
    ramp: usize,
}

impl Verdict {
    const PROTECT: Self = Self { target: 0.0, ramp: FALLBACK_RAMP };
    const RELEASE: Self = Self { target: 1.0, ramp: RELEASE_RAMP };
    const IMMEDIATE: Self = Self { target: 0.0, ramp: IMMEDIATE_RAMP };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Protected,
    Released,
}

/// Why a released path was taken away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    ReferenceIncomplete,
    Restarted,
    Reset,
    EchoInHighs,
    ReleasedHighsTooLarge,
    EchoInHighsUnderVoice,
    DelayChanged,
    WeakCancellation,
}

impl Fallback {
    fn reason(self) -> &'static str {
        match self {
            Self::ReferenceIncomplete => "playback reference incomplete",
            Self::Restarted => "audio stream restarted",
            Self::Reset => "echo path changed",
            Self::EchoInHighs => "microphone highs follow the playback highs",
            Self::ReleasedHighsTooLarge => "the released highs are large next to the microphone's",
            Self::EchoInHighsUnderVoice => "microphone highs follow the playback highs under a voice",
            Self::DelayChanged => "the echo delay moved",
            Self::WeakCancellation => "echo cancellation weakened while playback is active",
        }
    }

    /// Evidence gathered before an interrupted stream is still about the same echo path.
    fn keeps_evidence(self) -> bool {
        matches!(self, Self::ReferenceIncomplete | Self::Restarted)
    }
}

/// Why a complete set of evidence did not release the highs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hold {
    PlaybackHighsSteady,
    MicrophoneHighsSteady,
    EchoInHighs,
    ReleasedHighsTooLarge,
}

impl Hold {
    fn reason(self) -> &'static str {
        match self {
            Self::PlaybackHighsSteady => "the playback highs are too steady to compare with the microphone",
            Self::MicrophoneHighsSteady => "the microphone highs are too steady to compare with the playback",
            Self::EchoInHighs => "the microphone highs follow the playback highs",
            Self::ReleasedHighsTooLarge => "the released highs are large next to the microphone's",
        }
    }
}

/// Why the correlation could not be measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Inconclusive {
    Gathering,
    PlaybackSteady,
    ObservedSteady,
}

/// One frame of evidence: highs levels in dB, and the powers the means are taken over.
#[derive(Debug, Clone, Copy, Default)]
struct Evidence {
    /// The highs the playback may have leaked into: the microphone's, or the released path's.
    observed_highs: f64,
    /// Every playback series at [`LAGS`] delays around the estimate, series by series.
    playback_highs: [f64; PAIRS],
    microphone_power: f64,
    released_power: f64,
}

impl Evidence {
    /// The loudest playback series at the delay estimate.
    fn playback_level(&self) -> f64 {
        (0..SERIES).map(|series| self.playback_highs[series * LAGS + LAGS / 2]).fold(f64::from(FLOOR_DBFS), f64::max)
    }
}

/// Running sums over the window, so each frame costs a handful of additions.
#[derive(Debug, Clone, Copy, Default)]
struct Sums {
    observed: f64,
    observed_squared: f64,
    playback: [f64; PAIRS],
    playback_squared: [f64; PAIRS],
    cross: [f64; PAIRS],
    microphone_power: f64,
    released_power: f64,
}

impl Sums {
    fn accumulate(&mut self, evidence: &Evidence, sign: f64) {
        self.observed += sign * evidence.observed_highs;
        self.observed_squared += sign * evidence.observed_highs.powi(2);
        for pair in 0..PAIRS {
            let playback = evidence.playback_highs[pair];
            self.playback[pair] += sign * playback;
            self.playback_squared[pair] += sign * playback.powi(2);
            self.cross[pair] += sign * playback * evidence.observed_highs;
        }
        self.microphone_power += sign * evidence.microphone_power;
        self.released_power += sign * evidence.released_power;
    }
}

/// The last `N` frames of evidence, in a preallocated ring.
#[derive(Debug)]
struct EvidenceWindow<const N: usize> {
    frames: Vec<Evidence>,
    next: usize,
    len: usize,
    sums: Sums,
    /// The delay estimate, in frames, the stored evidence was paired with.
    delay_frames: usize,
}

impl<const N: usize> EvidenceWindow<N> {
    fn new() -> Self {
        Self { frames: vec![Evidence::default(); N], next: 0, len: 0, sums: Sums::default(), delay_frames: 0 }
    }

    fn clear(&mut self) {
        self.next = 0;
        self.len = 0;
        self.sums = Sums::default();
    }

    fn is_full(&self) -> bool {
        self.len == N
    }

    /// The stored evidence, if any, was paired with a delay within a frame of `delay_frames`.
    fn fits_delay(&self, delay_frames: usize) -> bool {
        self.len == 0 || self.delay_frames.abs_diff(delay_frames) <= 1
    }

    fn push(&mut self, evidence: Evidence) {
        if self.is_full() {
            let oldest = self.frames[self.next];
            self.sums.accumulate(&oldest, -1.0);
        }
        self.sums.accumulate(&evidence, 1.0);
        self.frames[self.next] = evidence;
        self.next = (self.next + 1) % N;
        self.len = (self.len + 1).min(N);
        if self.next == 0 {
            self.recompute();
        }
    }

    /// Once per lap, so rounding in the running sums cannot accumulate.
    fn recompute(&mut self) {
        let mut sums = Sums::default();
        for evidence in &self.frames[..self.len] {
            sums.accumulate(evidence, 1.0);
        }
        self.sums = sums;
    }

    /// The strongest correlation between the observed highs' and the playback's highs
    /// envelopes across the playback series and delays tried; the strongest is the one that
    /// best explains echo. A series too steady to correlate is left out.
    fn correlation(&self) -> Result<f64, Inconclusive> {
        if !self.is_full() {
            return Err(Inconclusive::Gathering);
        }
        let n = self.len as f64;
        let sums = &self.sums;
        let observed_variance = (sums.observed_squared / n - (sums.observed / n).powi(2)).max(0.0);
        if observed_variance < MIN_OBSERVED_VARIANCE {
            return Err(Inconclusive::ObservedSteady);
        }
        let variance = |pair: usize| (sums.playback_squared[pair] / n - (sums.playback[pair] / n).powi(2)).max(0.0);
        let varies = |series: usize| variance(series * LAGS + LAGS / 2).sqrt() >= MIN_PLAYBACK_STD_DB;
        let correlation = |pair: usize| {
            let covariance = sums.cross[pair] / n - sums.observed / n * (sums.playback[pair] / n);
            covariance / (observed_variance.sqrt() * variance(pair).sqrt())
        };
        let best = (0..SERIES)
            .filter(|&series| varies(series))
            .flat_map(|series| (0..LAGS).map(move |lag| series * LAGS + lag))
            .filter(|&pair| variance(pair) > 0.0)
            .map(correlation)
            .reduce(f64::max);
        best.ok_or(Inconclusive::PlaybackSteady)
    }

    /// Mean released-highs level and mean microphone-highs level over the window, in dBFS.
    fn mean_levels(&self) -> (f32, f32) {
        let n = self.len.max(1) as f64;
        let level = |power: f64| (power_db(power / n) as f32).max(FLOOR_DBFS);
        (level(self.sums.released_power), level(self.sums.microphone_power))
    }
}

/// The last [`HISTORY_FRAMES`] playback highs levels, every series.
#[derive(Debug)]
struct PlaybackHistory {
    levels: [[f32; SERIES]; HISTORY_FRAMES],
    next: usize,
}

impl PlaybackHistory {
    fn new() -> Self {
        Self { levels: [[FLOOR_DBFS; SERIES]; HISTORY_FRAMES], next: 0 }
    }

    fn push(&mut self, levels: [f32; SERIES]) {
        self.levels[self.next] = levels;
        self.next = (self.next + 1) % HISTORY_FRAMES;
    }

    /// The levels pushed `age` frames before the latest ones.
    fn at(&self, age: usize) -> [f32; SERIES] {
        let age = age.min(HISTORY_FRAMES - 1);
        self.levels[(self.next + 2 * HISTORY_FRAMES - 1 - age) % HISTORY_FRAMES]
    }
}

/// Decides, frame by frame, whether the call hears the released path.
///
/// It starts protected and moves only on positive evidence, gathered on frames where the
/// microphone holds echo rather than a voice. Fallback checks run on every frame, voiced or
/// not, so an unsafe released path never rides out a whole utterance. Under a voice the
/// released highs are expected to be large, so the check there is whether the microphone's
/// highs follow the playback: a separate, shorter window of every playback-active frame.
/// The released path's own highs cannot show it: AEC3 gains follow the playback level, so
/// even a voice without echo moves with the playback, and echo AEC3 cancels does not.
#[derive(Debug)]
struct ReleaseGuard {
    release_correlation: f64,
    phase: Phase,
    /// Voice-free frames: the evidence that releases the highs.
    window: EvidenceWindow<WINDOW_FRAMES>,
    /// Playback-active frames, voiced or not: the microphone's highs against the playback.
    watch: EvidenceWindow<WATCH_FRAMES>,
    history: PlaybackHistory,
    delay_frames: usize,
    learned_frames: u32,
    since_fallback: u32,
    unsafe_run: u32,
    /// A reason for staying on Strong has been logged this session.
    explained: bool,
}

impl ReleaseGuard {
    fn new(with_ai: bool) -> Self {
        Self {
            release_correlation: if with_ai { RELEASE_CORRELATION_AI } else { RELEASE_CORRELATION },
            phase: Phase::Protected,
            window: EvidenceWindow::new(),
            watch: EvidenceWindow::new(),
            history: PlaybackHistory::new(),
            delay_frames: 0,
            learned_frames: 0,
            since_fallback: 0,
            unsafe_run: 0,
            explained: false,
        }
    }

    /// Forget everything measured: the echo path may have changed.
    fn reset(&mut self) {
        self.fall_back(Fallback::Reset);
        self.window.clear();
        self.watch.clear();
        self.history = PlaybackHistory::new();
        self.delay_frames = 0;
    }

    /// Return to Strong after an interrupted stream. The echo path is unchanged, so the
    /// window stays; AEC3 retrained, so its learning is checked again.
    fn restart(&mut self) {
        self.fall_back(Fallback::Restarted);
    }

    /// Take in one frame and say how far the call may hear the released path.
    /// `metrics` are AEC3's statistics, read every [`METRICS_INTERVAL`] frames.
    fn update(&mut self, levels: &FrameLevels, metrics: Option<&Metrics>) -> Verdict {
        self.history.push(levels.playback_highs);
        self.since_fallback = self.since_fallback.saturating_add(1);
        if let Some(metrics) = metrics {
            self.read_metrics(metrics);
            if levels.playback > PLAYBACK_ACTIVE_DBFS {
                self.discard_evidence_for_other_delay();
            }
        }
        if !levels.complete {
            self.fall_back(Fallback::ReferenceIncomplete);
            return Verdict::IMMEDIATE;
        }
        self.add_watch(levels);
        let is_evidence = levels.is_evidence();
        if is_evidence {
            self.add_evidence(levels);
        }
        match self.phase {
            Phase::Protected if is_evidence => self.consider_release(),
            Phase::Protected => {}
            Phase::Released => {
                if let Some(reason) = self.unsafe_reason(levels, metrics) {
                    self.fall_back(reason);
                }
            }
        }
        match self.phase {
            Phase::Protected => Verdict::PROTECT,
            Phase::Released => Verdict::RELEASE,
        }
    }

    fn read_metrics(&mut self, metrics: &Metrics) {
        let learned =
            metrics.echo_return_loss_enhancement_db >= LEARNED_ERLE_DB || metrics.echo_return_loss_db >= LEARNED_ERL_DB;
        self.learned_frames = if learned { self.learned_frames.saturating_add(METRICS_INTERVAL as u32) } else { 0 };
        let delay_frames = (metrics.delay_ms.max(0) + FRAME_MS / 2) / FRAME_MS;
        self.delay_frames = (delay_frames as usize).min(MAX_DELAY_FRAMES);
    }

    /// Evidence paired with a delay that has since moved no longer describes the echo path,
    /// whatever kind of frame the move was noticed on. Released highs lose their evidence too.
    fn discard_evidence_for_other_delay(&mut self) {
        if self.window.fits_delay(self.delay_frames) && self.watch.fits_delay(self.delay_frames) {
            return;
        }
        match self.phase {
            Phase::Released => self.fall_back(Fallback::DelayChanged),
            Phase::Protected => {
                self.window.clear();
                self.watch.clear();
            }
        }
    }

    /// The playback highs, every series, at [`LAGS`] delays around the estimate.
    fn delayed_playback(&self) -> [f64; PAIRS] {
        let mut delayed = [0.0; PAIRS];
        for lag in 0..LAGS {
            let age = (self.delay_frames + lag).saturating_sub(1);
            for (series, level) in self.history.at(age).into_iter().enumerate() {
                delayed[series * LAGS + lag] = f64::from(level);
            }
        }
        delayed
    }

    fn add_evidence(&mut self, levels: &FrameLevels) {
        self.window.delay_frames = self.delay_frames;
        self.window.push(Evidence {
            observed_highs: f64::from(levels.microphone_highs),
            playback_highs: self.delayed_playback(),
            microphone_power: db_to_power(levels.microphone_highs),
            released_power: db_to_power(levels.released_highs),
        });
    }

    /// Record how the microphone's highs move against the playback on every frame the
    /// playback highs are audible, whether or not a voice is present.
    fn add_watch(&mut self, levels: &FrameLevels) {
        let playback_highs = self.delayed_playback();
        let evidence =
            Evidence { observed_highs: f64::from(levels.microphone_highs), playback_highs, ..Evidence::default() };
        if evidence.playback_level() <= f64::from(PLAYBACK_HIGHS_FLOOR_DBFS) {
            return;
        }
        self.watch.delay_frames = self.delay_frames;
        self.watch.push(evidence);
    }

    /// Release the highs if every piece of positive evidence is in. Staying protected
    /// because evidence is still arriving is silent; staying protected because the evidence
    /// is against release, or unreadable, is explained once.
    fn consider_release(&mut self) {
        if !self.window.is_full() || self.learned_frames < LEARNED_FRAMES || self.since_fallback < DWELL_FRAMES {
            return;
        }
        let correlation = match self.window.correlation() {
            Ok(correlation) => correlation,
            Err(Inconclusive::Gathering) => return,
            Err(Inconclusive::PlaybackSteady) => return self.explain(Hold::PlaybackHighsSteady),
            Err(Inconclusive::ObservedSteady) => return self.explain(Hold::MicrophoneHighsSteady),
        };
        if correlation > self.release_correlation {
            return self.explain(Hold::EchoInHighs);
        }
        let (released, microphone) = self.window.mean_levels();
        if released > AUDIBLE_RELEASED_DBFS && released > microphone - RELEASE_MARGIN_DB {
            return self.explain(Hold::ReleasedHighsTooLarge);
        }
        log::info!(
            "Adaptive echo removal: releasing the voice highs (microphone and playback highs correlate at {correlation:.2})"
        );
        self.phase = Phase::Released;
        self.unsafe_run = 0;
    }

    fn explain(&mut self, hold: Hold) {
        if !self.explained {
            self.explained = true;
            log::info!("Adaptive echo removal: staying on strong protection ({})", hold.reason());
        }
    }

    /// Fallback checks run on every frame, voiced or not.
    fn unsafe_reason(&mut self, levels: &FrameLevels, metrics: Option<&Metrics>) -> Option<Fallback> {
        let weak = metrics.is_some_and(|metrics| {
            metrics.echo_return_loss_enhancement_db < FALLBACK_ERLE_DB && metrics.echo_return_loss_db < FALLBACK_ERL_DB
        });
        if weak && levels.playback > PLAYBACK_ACTIVE_DBFS {
            return Some(Fallback::WeakCancellation);
        }
        let too_much = levels.is_voice_free() && levels.releases_too_much();
        self.unsafe_run = if too_much { self.unsafe_run + 1 } else { 0 };
        if self.unsafe_run >= FALLBACK_FRAMES {
            return Some(Fallback::ReleasedHighsTooLarge);
        }
        if follows_playback(self.window.correlation(), FALLBACK_CORRELATION) {
            return Some(Fallback::EchoInHighs);
        }
        follows_playback(self.watch.correlation(), WATCH_CORRELATION).then_some(Fallback::EchoInHighsUnderVoice)
    }

    fn fall_back(&mut self, reason: Fallback) {
        if self.phase == Phase::Released {
            log::info!("Adaptive echo removal: back to strong protection ({})", reason.reason());
        }
        self.phase = Phase::Protected;
        self.since_fallback = 0;
        self.learned_frames = 0;
        self.unsafe_run = 0;
        self.watch.clear();
        if !reason.keeps_evidence() {
            self.window.clear();
        }
    }
}

/// A conclusive correlation at or over `threshold`. An inconclusive one proves nothing, and
/// the guard cannot tell speech from echo, so it is not read as either.
fn follows_playback(correlation: Result<f64, Inconclusive>, threshold: f64) -> bool {
    correlation.is_ok_and(|correlation| correlation >= threshold)
}

fn db_to_power(db: f32) -> f64 {
    10f64.powf(f64::from(db) / 10.0)
}

/// Quality factors of a fourth-order Butterworth filter's two sections.
const BUTTERWORTH_4_Q: [f64; 2] = [0.541_196_1, 1.306_563];

/// One second-order high-pass section.
#[derive(Debug)]
struct Section {
    b0: f64,
    b1: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Section {
    fn new(cutoff_hz: f64, quality: f64) -> Self {
        let omega = std::f64::consts::TAU * cutoff_hz / f64::from(RATE);
        let alpha = omega.sin() / (2.0 * quality);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 + omega.cos()) / 2.0 / a0;
        Self { b0, b1: -2.0 * b0, a1: -2.0 * omega.cos() / a0, a2: (1.0 - alpha) / a0, z1: 0.0, z2: 0.0 }
    }

    fn process(&mut self, input: f64) -> f64 {
        let output = self.b0 * input + self.z1;
        self.z1 = self.b1 * input - self.a1 * output + self.z2;
        self.z2 = self.b0 * input - self.a2 * output;
        output
    }
}

/// A fourth-order high-pass filter. It is steep enough that the lows, where a headset leak
/// is strongest, do not pass as highs.
#[derive(Debug)]
struct HighPass {
    sections: [Section; 2],
}

impl HighPass {
    fn new(cutoff_hz: f64) -> Self {
        Self { sections: BUTTERWORTH_4_Q.map(|quality| Section::new(cutoff_hz, quality)) }
    }

    fn clear(&mut self) {
        *self = Self::new(HIGHS_CUTOFF_HZ);
    }

    /// Mean power of `samples` above the cutoff.
    fn power(&mut self, samples: &[f32; FRAME]) -> f64 {
        let mut total = 0.0;
        for &sample in samples {
            let output = self.sections.iter_mut().fold(f64::from(sample), |signal, section| section.process(signal));
            total += output * output;
        }
        total / FRAME as f64
    }
}

/// Moves from Strong's output towards the released path's, never beyond either.
#[derive(Debug, Default)]
struct Crossfade {
    weight: f32,
}

impl Crossfade {
    /// Blend `released` into `strong` in place, one sample at a time.
    fn mix(&mut self, strong: &mut [f32; FRAME], released: &[f32; FRAME], verdict: Verdict) {
        if self.weight == 0.0 && verdict.target == 0.0 {
            return;
        }
        let step = 1.0 / verdict.ramp as f32;
        for (strong, &released) in strong.iter_mut().zip(released) {
            // Landing on the target within a step and a half keeps float rounding from
            // leaving a ramp of exactly `ramp` samples a hair short.
            let remaining = verdict.target - self.weight;
            self.weight =
                if remaining.abs() <= 1.5 * step { verdict.target } else { self.weight + step.copysign(remaining) };
            if self.weight > 0.0 {
                *strong += self.weight * (released - *strong);
            }
        }
    }
}

/// Adaptive's second AEC3 core and its guard.
#[derive(Debug)]
pub(crate) struct Release {
    aec3: EchoProcessor,
    guard: ReleaseGuard,
    crossfade: Crossfade,
    frames: u64,
    microphone_highs: HighPass,
    left_highs: HighPass,
    right_highs: HighPass,
    released_highs: HighPass,
    clean: Vec<f32>,
    released: [f32; FRAME],
    left: [f32; FRAME],
    right: [f32; FRAME],
    difference: [f32; FRAME],
}

impl Release {
    pub(crate) fn new(config: &Config, with_ai: bool) -> Result<Self, Error> {
        Ok(Self {
            aec3: EchoProcessor::new(config)?,
            guard: ReleaseGuard::new(with_ai),
            crossfade: Crossfade::default(),
            frames: 0,
            microphone_highs: HighPass::new(HIGHS_CUTOFF_HZ),
            left_highs: HighPass::new(HIGHS_CUTOFF_HZ),
            right_highs: HighPass::new(HIGHS_CUTOFF_HZ),
            released_highs: HighPass::new(HIGHS_CUTOFF_HZ),
            clean: vec![0.0; 2 * FRAME],
            released: [0.0; FRAME],
            left: [0.0; FRAME],
            right: [0.0; FRAME],
            difference: [0.0; FRAME],
        })
    }

    /// Forget the echo path, as after a realignment: Strong alone until relearned.
    pub(crate) fn reset(&mut self) {
        self.aec3.reset();
        self.guard.reset();
        self.crossfade = Crossfade::default();
        self.clear_filters();
    }

    /// Clear interrupted stream history: Strong alone until AEC3's learning is checked again.
    pub(crate) fn restart_stream(&mut self) {
        self.aec3.reset();
        self.guard.restart();
        self.crossfade = Crossfade::default();
        self.clear_filters();
    }

    fn clear_filters(&mut self) {
        self.microphone_highs.clear();
        self.left_highs.clear();
        self.right_highs.clear();
        self.released_highs.clear();
    }

    /// Run the second core on the audio the first one saw, and blend its output into
    /// `strong`, the first core's mono output, as the guard allows.
    ///
    /// `microphone` is the raw mono microphone; `near` and `far` are the interleaved stereo
    /// frames both cores process; `primary` supplies the statistics.
    pub(crate) fn process(
        &mut self,
        primary: &EchoProcessor,
        microphone: &[f32; FRAME],
        near: &[f32],
        far: &[f32],
        complete: bool,
        strong: &mut [f32; FRAME],
    ) -> Result<(), Error> {
        self.aec3.process(near, far, &mut self.clean)?;
        for (mono, [left, right]) in self.released.iter_mut().zip(self.clean.as_chunks::<2>().0) {
            *mono = 0.5 * (left + right);
        }
        for ((left, right), [far_left, far_right]) in
            self.left.iter_mut().zip(self.right.iter_mut()).zip(far.as_chunks::<2>().0)
        {
            *left = *far_left;
            *right = *far_right;
        }
        for ((difference, released), strong) in self.difference.iter_mut().zip(&self.released).zip(&*strong) {
            *difference = released - strong;
        }
        let level = |power: f64| (power_db(power) as f32).max(FLOOR_DBFS);
        let (left, right) = (self.left_highs.power(&self.left), self.right_highs.power(&self.right));
        let levels = FrameLevels {
            complete,
            microphone: dbfs(microphone),
            playback: dbfs(far),
            strong: dbfs(strong),
            released: dbfs(&self.released),
            microphone_highs: level(self.microphone_highs.power(microphone)),
            playback_highs: [level(left), level(right), level(0.5 * (left + right))],
            released_highs: level(self.released_highs.power(&self.difference)),
        };
        let metrics = self.frames.is_multiple_of(METRICS_INTERVAL).then(|| primary.metrics());
        self.frames += 1;
        let verdict = self.guard.update(&levels, metrics.as_ref());
        self.crossfade.mix(strong, &self.released, verdict);
        Ok(())
    }

    /// How far the call hears the second core, from 0 to 1.
    #[cfg(test)]
    pub(crate) fn weight(&self) -> f32 {
        self.crossfade.weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A converged canceller: AEC3 reports 6.0 dB ERLE and 20 dB ERL at best.
    fn learned() -> Metrics {
        Metrics { echo_return_loss_db: 20.0, echo_return_loss_enhancement_db: 6.0, delay_ms: 0 }
    }

    fn unlearned() -> Metrics {
        Metrics { echo_return_loss_db: 10.0, echo_return_loss_enhancement_db: 3.0, delay_ms: 0 }
    }

    /// A leak that is quiet but not cancelled: low ERLE, yet the echo path loses 19 dB.
    fn weak_leak() -> Metrics {
        Metrics { echo_return_loss_db: 19.0, echo_return_loss_enhancement_db: 3.0, delay_ms: 0 }
    }

    /// An echo-only frame: the microphone holds the leak, Strong removed 15 dB of it.
    fn echo_only(playback_highs: f32, microphone_highs: f32) -> FrameLevels {
        FrameLevels {
            complete: true,
            microphone: -30.0,
            playback: -25.0,
            strong: -45.0,
            released: -45.0,
            microphone_highs,
            playback_highs: [playback_highs; SERIES],
            released_highs: FLOOR_DBFS,
        }
    }

    /// A frame with a voice: Strong leaves most of the microphone in place.
    fn voiced(playback_highs: f32, microphone_highs: f32) -> FrameLevels {
        FrameLevels { strong: -32.0, released: -32.0, ..echo_only(playback_highs, microphone_highs) }
    }

    /// Standardized pseudo-random values: zero mean and unit variance over their length.
    fn standardized(seed: u32) -> Vec<f64> {
        let mut state = seed;
        let values: Vec<f64> = (0..WINDOW_FRAMES)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                f64::from(state >> 8) / f64::from(1u32 << 24) - 0.5
            })
            .collect();
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
        values.iter().map(|v| (v - mean) / variance.sqrt()).collect()
    }

    /// Playback and microphone highs envelopes (dB) that repeat every [`WINDOW_FRAMES`] and
    /// have exactly the sample correlation `rho` over each repeat.
    struct Highs {
        playback: Vec<f64>,
        microphone: Vec<f64>,
    }

    impl Highs {
        fn correlated(rho: f64) -> Self {
            let playback = standardized(1);
            let mut other = standardized(2);
            let shared = other.iter().zip(&playback).map(|(a, b)| a * b).sum::<f64>() / playback.len() as f64;
            other.iter_mut().zip(&playback).for_each(|(a, b)| *a -= shared * b);
            let scale = (other.iter().map(|a| a * a).sum::<f64>() / other.len() as f64).sqrt();
            let microphone = playback
                .iter()
                .zip(&other)
                .map(|(p, o)| -60.0 + 3.0 * (rho * p + (1.0 - rho * rho).sqrt() * o / scale));
            Self { microphone: microphone.collect(), playback: playback.iter().map(|p| -40.0 + 6.0 * p).collect() }
        }

        fn echo_only(&self, frame: usize) -> FrameLevels {
            let at = frame % WINDOW_FRAMES;
            echo_only(self.playback[at] as f32, self.microphone[at] as f32)
        }

        fn voiced(&self, frame: usize) -> FrameLevels {
            let at = frame % WINDOW_FRAMES;
            voiced(self.playback[at] as f32, self.microphone[at] as f32)
        }
    }

    /// A guard fed frames with AEC3 statistics every [`METRICS_INTERVAL`] frames.
    struct Driver {
        guard: ReleaseGuard,
        metrics: Metrics,
        frame: usize,
    }

    impl Driver {
        fn new(with_ai: bool) -> Self {
            Self { guard: ReleaseGuard::new(with_ai), metrics: learned(), frame: 0 }
        }

        fn step(&mut self, levels: FrameLevels) -> Verdict {
            let metrics = (self.frame as u64).is_multiple_of(METRICS_INTERVAL).then_some(self.metrics);
            self.frame += 1;
            self.guard.update(&levels, metrics.as_ref())
        }

        /// Feed up to `frames` frames; report how many frames in the highs were released.
        fn first_release(&mut self, frames: usize, mut levels: impl FnMut(usize) -> FrameLevels) -> Option<usize> {
            (0..frames).find(|_| {
                let frame = self.frame;
                self.step(levels(frame)).target == 1.0
            })
        }

        fn released(&self) -> bool {
            self.guard.phase == Phase::Released
        }
    }

    fn released_driver(highs: &Highs, with_ai: bool) -> Driver {
        let mut driver = Driver::new(with_ai);
        driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("uncorrelated highs release");
        driver
    }

    #[test]
    fn it_starts_protected_and_waits_for_evidence_learning_and_dwell() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        assert_eq!(driver.step(highs.echo_only(0)), Verdict::PROTECT);
        driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("released");
        let frames = driver.frame;
        assert!(frames >= DWELL_FRAMES as usize, "released after only {frames} frames");
        assert!(frames < 600, "released only after {frames} frames");
        assert!(driver.released());
    }

    #[test]
    fn quiet_or_constant_playback_highs_stay_strong_and_are_explained_once() {
        let steady = |_: usize| echo_only(-40.0, -60.0);
        let mut driver = Driver::new(false);
        assert_eq!(driver.first_release(1_500, steady), None);
        assert!(driver.guard.window.is_full());
        assert!(driver.guard.explained, "staying on Strong needs a logged reason");
        let quiet = |frame: usize| echo_only(-70.0 + (frame % 7) as f32, -60.0);
        let mut driver = Driver::new(false);
        assert_eq!(driver.first_release(1_500, quiet), None);
        assert_eq!(driver.guard.window.len, 0, "inaudible playback highs are not evidence");
    }

    #[test]
    fn highs_that_follow_the_playback_highs_never_release() {
        for rho in [0.3, 0.6, 0.95] {
            let highs = Highs::correlated(rho);
            let mut driver = Driver::new(false);
            assert_eq!(driver.first_release(3_000, |frame| highs.echo_only(frame)), None, "rho {rho}");
        }
    }

    #[test]
    fn uncorrelated_varying_highs_release() {
        let highs = Highs::correlated(0.0);
        assert!(released_driver(&highs, false).released());
        assert!(released_driver(&highs, true).released());
    }

    #[test]
    fn ai_noise_removal_asks_for_weaker_correlation_before_release() {
        let highs = Highs::correlated(0.2);
        let mut with_ai = Driver::new(true);
        assert_eq!(with_ai.first_release(3_000, |frame| highs.echo_only(frame)), None);
        let mut without_ai = Driver::new(false);
        assert!(without_ai.first_release(3_000, |frame| highs.echo_only(frame)).is_some());
    }

    #[test]
    fn poor_learning_never_releases_and_interrupted_learning_starts_over() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = unlearned();
        assert_eq!(driver.first_release(1_500, |frame| highs.echo_only(frame)), None);
        driver.metrics = learned();
        for _ in 0..LEARNED_FRAMES - METRICS_INTERVAL as u32 {
            driver.step(highs.echo_only(driver.frame));
        }
        driver.metrics = unlearned();
        driver.step(highs.echo_only(driver.frame));
        assert_eq!(driver.guard.learned_frames, 0);
    }

    #[test]
    fn the_return_loss_figure_alone_can_show_learning() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = Metrics { echo_return_loss_db: 19.5, echo_return_loss_enhancement_db: 0.0, delay_ms: 0 };
        assert!(driver.first_release(1_500, |frame| highs.echo_only(frame)).is_some());
    }

    #[test]
    fn the_enhancement_figure_alone_can_show_learning() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = Metrics { echo_return_loss_db: 0.0, echo_return_loss_enhancement_db: 5.5, delay_ms: 0 };
        assert!(driver.first_release(1_500, |frame| highs.echo_only(frame)).is_some());
    }

    #[test]
    fn figures_just_under_the_learned_thresholds_never_release() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = Metrics { echo_return_loss_db: 18.9, echo_return_loss_enhancement_db: 5.4, delay_ms: 0 };
        assert_eq!(driver.first_release(1_500, |frame| highs.echo_only(frame)), None);
    }

    #[test]
    fn voiced_frames_do_not_count_towards_release() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        for _ in 0..DWELL_FRAMES {
            driver.step(highs.voiced(driver.frame));
        }
        assert_eq!(driver.guard.window.len, 0);
        for _ in 0..WINDOW_FRAMES - 1 {
            driver.step(highs.echo_only(driver.frame));
            driver.step(highs.voiced(driver.frame));
        }
        assert!(!driver.released(), "299 voice-free frames are not enough");
        driver.step(highs.echo_only(driver.frame));
        assert!(driver.released());
    }

    #[test]
    fn release_waits_for_a_voice_free_frame() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        for _ in 0..WINDOW_FRAMES {
            driver.step(highs.echo_only(driver.frame));
        }
        for _ in 0..DWELL_FRAMES {
            driver.step(highs.voiced(driver.frame));
        }
        assert!(!driver.released(), "the highs were released during a voice");
        driver.step(highs.echo_only(driver.frame));
        assert!(driver.released());
    }

    #[test]
    fn large_released_highs_on_voice_free_frames_fall_back_but_fewer_than_five_do_not() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let loud = FrameLevels { released_highs: -35.0, microphone_highs: -38.0, ..highs.echo_only(0) };
        for _ in 0..FALLBACK_FRAMES - 1 {
            assert_eq!(driver.step(loud), Verdict::RELEASE);
        }
        driver.step(highs.echo_only(0));
        for _ in 0..FALLBACK_FRAMES - 1 {
            driver.step(loud);
        }
        assert!(driver.released(), "the run of unsafe frames was broken");
        assert_eq!(driver.step(loud), Verdict::PROTECT);
        assert!(!driver.released());
    }

    #[test]
    fn a_voiced_frame_breaks_a_run_of_unsafe_frames_and_is_never_one_itself() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let loud = FrameLevels { released_highs: -35.0, microphone_highs: -38.0, ..highs.echo_only(0) };
        let restored = FrameLevels { strong: -32.0, released: -32.0, ..loud };
        for _ in 0..100 {
            assert_eq!(driver.step(restored), Verdict::RELEASE);
        }
        for _ in 0..FALLBACK_FRAMES - 1 {
            driver.step(loud);
        }
        driver.step(restored);
        for _ in 0..FALLBACK_FRAMES - 1 {
            assert_eq!(driver.step(loud), Verdict::RELEASE);
        }
        assert_eq!(driver.step(loud), Verdict::PROTECT);
    }

    #[test]
    fn restored_voice_highs_are_large_next_to_the_microphone_and_stay_released() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        for _ in 0..10 * WINDOW_FRAMES {
            let frame = driver.frame;
            // Strong removed 2.5 dB of the voice's highs; the released path puts them back.
            let voiced = highs.voiced(frame);
            let restored = FrameLevels { released_highs: voiced.microphone_highs - 2.5, ..voiced };
            assert_eq!(driver.step(restored), Verdict::RELEASE, "frame {frame}");
        }
    }

    #[test]
    fn microphone_highs_that_follow_the_playback_under_a_voice_fall_back_within_the_watch_window() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let echo = Highs::correlated(0.9);
        let fell_back = (1..=2 * WATCH_FRAMES).find(|_| {
            let levels = echo.voiced(driver.frame);
            driver.step(levels) == Verdict::PROTECT
        });
        let frames = fell_back.expect("the microphone highs followed the playback for two windows");
        assert!(frames <= WATCH_FRAMES, "fell back {frames} frames in");
        assert!(driver.guard.watch.len == 0 && driver.guard.window.len == 0);
    }

    #[test]
    fn the_watch_tolerates_a_correlation_that_a_short_window_shows_by_chance() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let between = Highs::correlated(0.6);
        for _ in 0..4 * WINDOW_FRAMES {
            let levels = between.voiced(driver.frame);
            assert_eq!(driver.step(levels), Verdict::RELEASE, "frame {}", driver.frame);
        }
    }

    #[test]
    fn the_watch_cannot_judge_playback_highs_that_do_not_vary() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let echo = Highs::correlated(0.95);
        for _ in 0..4 * WATCH_FRAMES {
            let steady = FrameLevels { playback_highs: [-40.0; SERIES], ..echo.voiced(driver.frame) };
            assert_eq!(driver.step(steady), Verdict::RELEASE);
        }
    }

    #[test]
    fn the_watch_cannot_judge_microphone_highs_that_do_not_vary() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        for _ in 0..4 * WATCH_FRAMES {
            let steady = FrameLevels { microphone_highs: -35.0, ..calm.voiced(driver.frame) };
            assert_eq!(driver.step(steady), Verdict::RELEASE);
        }
    }

    #[test]
    fn the_watch_uses_only_audible_playback_highs() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let echo = Highs::correlated(0.95);
        let before = driver.guard.watch.next;
        for _ in 0..4 * WATCH_FRAMES {
            let quiet =
                FrameLevels { playback_highs: [PLAYBACK_HIGHS_FLOOR_DBFS - 5.0; SERIES], ..echo.voiced(driver.frame) };
            driver.step(quiet);
        }
        assert!(driver.released());
        assert_eq!(driver.guard.watch.next, before, "inaudible playback highs entered the watch");
    }

    #[test]
    fn one_channel_whose_highs_follow_the_microphone_blocks_release_whatever_the_others_do() {
        let echo = Highs::correlated(0.8);
        let other = Highs::correlated(0.0);
        // Left carries the leak; right and the mean of both powers do not follow it.
        let stereo = |frame: usize| FrameLevels {
            playback_highs: [
                echo.echo_only(frame).playback_highs[0],
                other.echo_only(frame).playback_highs[0],
                other.echo_only(frame + 17).playback_highs[0],
            ],
            ..echo.echo_only(frame)
        };
        let mut driver = Driver::new(false);
        assert_eq!(driver.first_release(3_000, stereo), None);
        assert!(driver.guard.explained);
    }

    #[test]
    fn a_leak_from_one_channel_under_a_voice_is_watched_even_when_the_others_are_silent() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let echo = Highs::correlated(0.9);
        let left_only = |frame: usize| {
            let levels = echo.voiced(frame);
            FrameLevels { playback_highs: [levels.playback_highs[0], FLOOR_DBFS, FLOOR_DBFS], ..levels }
        };
        let fell_back = (1..=2 * WATCH_FRAMES).any(|_| {
            let levels = left_only(driver.frame);
            driver.step(levels) == Verdict::PROTECT
        });
        assert!(fell_back);
    }

    #[test]
    fn released_highs_far_under_the_microphone_highs_are_safe_in_quiet_and_loud_rooms() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        for frame in 0..100 {
            let level = |microphone_highs, released_highs| FrameLevels {
                microphone_highs,
                released_highs,
                ..highs.echo_only(frame)
            };
            driver.step(level(-30.0, -45.0));
            driver.step(level(-80.0, -75.0));
        }
        assert!(driver.released());
    }

    #[test]
    fn correlation_thresholds_have_hysteresis() {
        let calm = Highs::correlated(0.0);
        let mut driver = released_driver(&calm, false);
        let between = Highs::correlated(0.3);
        for _ in 0..2 * WINDOW_FRAMES {
            driver.step(between.echo_only(driver.frame));
        }
        assert!(driver.released(), "0.3 sits between the release and fallback thresholds");
        let echo = Highs::correlated(0.8);
        let mut verdict = Verdict::RELEASE;
        for _ in 0..2 * WINDOW_FRAMES {
            verdict = driver.step(echo.echo_only(driver.frame));
            if !driver.released() {
                break;
            }
        }
        assert_eq!(verdict, Verdict::PROTECT);
    }

    #[test]
    fn weak_cancellation_falls_back_only_while_playback_is_active() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        driver.metrics = unlearned();
        let silent = FrameLevels { playback: FLOOR_DBFS, ..highs.voiced(0) };
        for _ in 0..METRICS_INTERVAL {
            driver.step(silent);
        }
        assert!(driver.released(), "weak statistics without playback prove nothing");
        for _ in 0..METRICS_INTERVAL {
            driver.step(highs.voiced(0));
        }
        assert!(!driver.released());
    }

    #[test]
    fn a_weak_leak_with_high_return_loss_releases_and_stays_released() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = weak_leak();
        driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("high ERL shows learning");
        for _ in 0..2 * WINDOW_FRAMES {
            driver.step(highs.voiced(driver.frame));
            assert!(driver.released(), "low ERLE alone dropped the released highs");
        }
    }

    #[test]
    fn a_leak_that_loses_both_figures_falls_back_while_playback_is_active() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        driver.metrics = weak_leak();
        driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("released");
        driver.metrics = Metrics { echo_return_loss_db: 16.9, ..weak_leak() };
        for _ in 0..METRICS_INTERVAL {
            driver.step(highs.voiced(driver.frame));
        }
        assert!(!driver.released());
    }

    #[test]
    fn after_a_fallback_the_dwell_and_a_fresh_window_are_required() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let loud = FrameLevels { released_highs: -35.0, microphone_highs: -38.0, ..highs.echo_only(0) };
        for _ in 0..FALLBACK_FRAMES {
            driver.step(loud);
        }
        assert!(!driver.released());
        assert_eq!(driver.guard.window.len, 0);
        assert_eq!(driver.guard.watch.len, 0);
        let release = driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("released again");
        assert!(release + 1 >= DWELL_FRAMES as usize, "re-released {release} frames after the fallback");
    }

    #[test]
    fn a_missing_reference_reaches_strong_within_one_frame() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let verdict = driver.step(FrameLevels { complete: false, ..highs.echo_only(0) });
        assert_eq!(verdict, Verdict::IMMEDIATE);
        assert!(verdict.ramp <= FRAME);
        assert!(!driver.released());
        assert_eq!(driver.guard.learned_frames, 0);
    }

    #[test]
    fn restart_keeps_the_trusted_path_evidence_but_reset_clears_it() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        driver.guard.restart();
        assert!(!driver.released());
        assert_eq!(driver.guard.learned_frames, 0);
        assert!(driver.guard.window.is_full());
        let release = driver.first_release(1_500, |frame| highs.echo_only(frame)).expect("released after a restart");
        assert!(release + 1 >= DWELL_FRAMES as usize, "released {release} frames after the restart");
        driver.guard.reset();
        assert_eq!(driver.guard.window.len, 0);
        assert!(!driver.released());
    }

    #[test]
    fn a_changed_echo_delay_discards_the_window() {
        let highs = Highs::correlated(0.0);
        let mut driver = Driver::new(false);
        for _ in 0..WINDOW_FRAMES / 2 {
            driver.step(highs.echo_only(driver.frame));
        }
        driver.metrics = Metrics { delay_ms: 120, ..learned() };
        while !(driver.frame as u64).is_multiple_of(METRICS_INTERVAL) {
            driver.step(highs.echo_only(driver.frame));
        }
        let before = driver.guard.window.len;
        driver.step(highs.echo_only(driver.frame));
        assert!(driver.guard.window.len < before, "window kept evidence for a different delay");
    }

    /// Feed `frames` voiced frames, the first on a metrics frame, with `metrics` read.
    fn voiced_with(driver: &mut Driver, highs: &Highs, metrics: Metrics, frames: usize) -> Verdict {
        driver.metrics = metrics;
        while !(driver.frame as u64).is_multiple_of(METRICS_INTERVAL) {
            driver.step(highs.voiced(driver.frame));
        }
        let mut verdict = Verdict::RELEASE;
        for _ in 0..frames {
            verdict = driver.step(highs.voiced(driver.frame));
        }
        verdict
    }

    #[test]
    fn a_delay_that_moves_under_a_voice_returns_to_strong_and_discards_the_evidence() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let verdict = voiced_with(&mut driver, &highs, Metrics { delay_ms: 120, ..learned() }, 1);
        assert_eq!(verdict, Verdict::PROTECT);
        assert!(verdict.ramp > FRAME, "a moved delay needs no immediate cut");
        assert!(!driver.released());
        assert_eq!((driver.guard.window.len, driver.guard.watch.len), (0, 1));
        assert!(driver.guard.delay_frames > 1);
    }

    #[test]
    fn a_delay_within_one_frame_of_the_evidence_keeps_the_highs_released() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        let verdict = voiced_with(&mut driver, &highs, Metrics { delay_ms: 14, ..learned() }, 20);
        assert_eq!(verdict, Verdict::RELEASE);
        assert!(driver.guard.window.is_full());
    }

    #[test]
    fn a_delay_reading_without_playback_proves_nothing() {
        let highs = Highs::correlated(0.0);
        let mut driver = released_driver(&highs, false);
        driver.metrics = Metrics { delay_ms: 120, ..learned() };
        let silent = FrameLevels { playback: FLOOR_DBFS, ..highs.voiced(0) };
        for _ in 0..METRICS_INTERVAL {
            driver.step(silent);
        }
        assert!(driver.released());
        assert!(driver.guard.window.is_full());
        driver.step(highs.voiced(driver.frame));
        driver.metrics = Metrics { delay_ms: 120, ..learned() };
        voiced_with(&mut driver, &highs, Metrics { delay_ms: 120, ..learned() }, 1);
        assert!(!driver.released());
    }

    /// Evidence in which only the first playback series carries levels.
    fn first_series(lags: [f64; LAGS]) -> [f64; PAIRS] {
        let mut pairs = [0.0; PAIRS];
        pairs[..LAGS].copy_from_slice(&lags);
        pairs
    }

    #[test]
    fn running_sums_match_the_window_contents_after_many_laps() {
        let mut window = EvidenceWindow::<WINDOW_FRAMES>::new();
        let series = Highs::correlated(0.4);
        for frame in 0..5 * WINDOW_FRAMES + 123 {
            let at = frame % WINDOW_FRAMES;
            let lagged = |lag: usize| series.playback[(at + WINDOW_FRAMES - lag) % WINDOW_FRAMES];
            window.push(Evidence {
                observed_highs: series.microphone[at],
                playback_highs: first_series([lagged(1), lagged(0), lagged(2)]),
                microphone_power: 1.0,
                released_power: 0.0,
            });
        }
        let running = window.correlation().unwrap();
        window.recompute();
        assert!((running - window.correlation().unwrap()).abs() < 1e-9);
    }

    #[test]
    fn the_correlation_is_the_strongest_across_the_delays_tried() {
        let series = Highs::correlated(0.0);
        let mut window = EvidenceWindow::<WINDOW_FRAMES>::new();
        for at in 0..WINDOW_FRAMES {
            // The microphone follows the playback one frame later than the estimate.
            let microphone = series.playback[(at + WINDOW_FRAMES - 1) % WINDOW_FRAMES];
            let lagged = |lag: usize| series.playback[(at + WINDOW_FRAMES - lag) % WINDOW_FRAMES];
            window.push(Evidence {
                observed_highs: microphone,
                playback_highs: first_series([lagged(0), lagged(1), lagged(2)]),
                microphone_power: 1.0,
                released_power: 0.0,
            });
        }
        assert!(window.correlation().unwrap() > 0.99);
    }

    #[test]
    fn playback_history_covers_the_longest_delay_with_a_frame_to_spare() {
        let mut history = PlaybackHistory::new();
        for level in 0..60 {
            history.push([level as f32; SERIES]);
        }
        assert_eq!(history.at(0), [59.0; SERIES]);
        assert_eq!(history.at(MAX_DELAY_FRAMES + 1), [(59 - MAX_DELAY_FRAMES - 1) as f32; SERIES]);
        assert_eq!(history.at(1_000), history.at(HISTORY_FRAMES - 1));
    }

    #[test]
    fn the_high_pass_removes_the_lows_and_keeps_the_highs() {
        let tone = |hz: f64| -> [f32; FRAME] {
            std::array::from_fn(|i| (std::f64::consts::TAU * hz * i as f64 / f64::from(RATE)).sin() as f32)
        };
        let power_of = |hz: f64| {
            let mut filter = HighPass::new(HIGHS_CUTOFF_HZ);
            let samples = tone(hz);
            (0..5).map(|_| filter.power(&samples)).last().unwrap()
        };
        let low = 10.0 * (power_of(1_000.0) / 0.5).log10();
        let high = 10.0 * (power_of(12_000.0) / 0.5).log10();
        assert!(low < -40.0, "1 kHz left at {low:.1} dB");
        assert!(high > -0.5, "12 kHz left at {high:.1} dB");
    }

    #[test]
    fn the_crossfade_is_convex_continuous_and_never_boosts() {
        let strong: [f32; FRAME] = std::array::from_fn(|i| 0.5 * (i as f32 * 0.1).sin());
        let released: [f32; FRAME] = std::array::from_fn(|i| 0.5 * (i as f32 * 0.37).cos());
        let mut crossfade = Crossfade::default();
        let mut previous = 0.0;
        for _ in 0..RELEASE_RAMP / FRAME + 2 {
            let mut mixed = strong;
            crossfade.mix(&mut mixed, &released, Verdict::RELEASE);
            for ((&mixed, &strong), &released) in mixed.iter().zip(&strong).zip(&released) {
                assert!(mixed >= strong.min(released) - 1e-6 && mixed <= strong.max(released) + 1e-6);
            }
            assert!(crossfade.weight >= previous, "the weight turned back while releasing");
            assert!(crossfade.weight - previous <= FRAME as f32 / RELEASE_RAMP as f32 + 1e-4);
            previous = crossfade.weight;
        }
        assert_eq!(crossfade.weight, 1.0);
        let mut mixed = strong;
        crossfade.mix(&mut mixed, &strong, Verdict::RELEASE);
        assert_eq!(mixed, strong, "identical paths must pass unchanged");
    }

    #[test]
    fn the_crossfade_returns_to_strong_within_the_requested_ramp() {
        let strong = [0.25; FRAME];
        let released = [-0.25; FRAME];
        for (verdict, frames) in [(Verdict::PROTECT, FALLBACK_RAMP.div_ceil(FRAME)), (Verdict::IMMEDIATE, 1)] {
            let mut crossfade = Crossfade { weight: 1.0 };
            for _ in 0..frames {
                crossfade.mix(&mut strong.clone(), &released, verdict);
            }
            assert_eq!(crossfade.weight, 0.0);
        }
    }

    #[test]
    fn a_protected_crossfade_leaves_strong_bit_exact() {
        let mut strong: [f32; FRAME] = std::array::from_fn(|i| if i == 3 { -0.0 } else { i as f32 * 1e-3 });
        let original = strong;
        Crossfade::default().mix(&mut strong, &[f32::MAX; FRAME], Verdict::PROTECT);
        assert!(strong.iter().zip(&original).all(|(a, b)| a.to_bits() == b.to_bits()));
    }
}
