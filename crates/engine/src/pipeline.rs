//! The per-frame processing chain, in Chrome's order: leak removal, then noise removal.

use echobridge_aec3::EchoProcessor;
use echobridge_denoise::Denoiser;
use echobridge_dsp::limiter::Limiter;
use echobridge_dsp::linear::{LinearCanceller, LinearConfig};
use echobridge_dsp::{FRAME, Stereo};

use crate::options::{EchoMode, NoiseRemoval, Options};
use crate::release::Release;

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error("echo cancellation could not start: {0}")]
    Aec3(#[from] echobridge_aec3::Error),
    #[error(transparent)]
    Denoise(#[from] echobridge_denoise::Error),
    #[error(transparent)]
    Dsp(#[from] echobridge_dsp::DspError),
}

/// Processing stages for one set of options, built off the audio thread: loading the AI
/// model takes about 0.3 s, which would interrupt the voice.
#[derive(Debug)]
pub struct Stages {
    options: Options,
    aec3: EchoProcessor,
    linear: Option<LinearCanceller>,
    /// Adaptive's second canceller; it starts out unheard.
    release: Option<Release>,
    /// `None` keeps the running pipeline's model, so switching modes with AI noise removal
    /// on does not reload it.
    denoiser: Option<Option<Denoiser>>,
}

impl Stages {
    /// Stages for `options`, replacing a pipeline currently running `current`.
    pub fn prepare(options: Options, current: Option<&Options>) -> Result<Self, PipelineError> {
        let wants_ai = options.noise == NoiseRemoval::Ai;
        let has_ai = current.is_some_and(|c| c.noise == NoiseRemoval::Ai);
        let denoiser = match (wants_ai, has_ai) {
            (true, true) => None,
            (true, false) => Some(Some(Denoiser::new(Default::default())?)),
            (false, _) => Some(None),
        };
        let release = options.release_config().map(|config| Release::new(&config, wants_ai)).transpose()?;
        Ok(Self {
            options,
            aec3: EchoProcessor::new(&options.aec3_config())?,
            linear: (options.echo == EchoMode::CleanVoice).then(|| LinearCanceller::new(LinearConfig::default())),
            release,
            denoiser,
        })
    }
}

/// Removes the playback leak and noise from 10 ms microphone frames.
#[derive(Debug)]
pub struct Pipeline {
    options: Options,
    aec3: EchoProcessor,
    linear: Option<LinearCanceller>,
    release: Option<Release>,
    denoiser: Option<Denoiser>,
    limiter: Limiter,
    // Working buffers: AEC3 runs on stereo, interleaved.
    near: Vec<f32>,
    far: Vec<f32>,
    clean: Vec<f32>,
    mono: [f32; FRAME],
}

impl Pipeline {
    pub fn new(options: Options) -> Result<Self, PipelineError> {
        let stages = Stages::prepare(options, None)?;
        Ok(Self {
            options,
            aec3: stages.aec3,
            linear: stages.linear,
            release: stages.release,
            denoiser: stages.denoiser.flatten(),
            limiter: Limiter::default(),
            near: vec![0.0; 2 * FRAME],
            far: vec![0.0; 2 * FRAME],
            clean: vec![0.0; 2 * FRAME],
            mono: [0.0; FRAME],
        })
    }

    pub fn options(&self) -> Options {
        self.options
    }

    /// Switch to prepared stages without interrupting the audio.
    pub fn apply(&mut self, stages: Stages) {
        self.options = stages.options;
        self.aec3 = stages.aec3;
        self.linear = stages.linear;
        self.release = stages.release;
        if let Some(denoiser) = stages.denoiser {
            self.denoiser = denoiser;
        }
    }

    /// Retrain after a realignment or resume. The AI model stays loaded.
    pub fn reset(&mut self) {
        self.aec3.reset();
        if let Some(linear) = &mut self.linear {
            linear.reset();
        }
        if let Some(release) = &mut self.release {
            release.reset();
        }
    }

    /// Clear interrupted stream history while retaining the accepted Clean voice filter.
    pub(crate) fn restart_stream(&mut self) {
        self.aec3.reset();
        if let Some(linear) = &mut self.linear {
            linear.clear_history();
        }
        if let Some(release) = &mut self.release {
            release.restart_stream();
        }
    }

    /// How far the call hears Adaptive's second canceller, from 0 to 1.
    #[cfg(test)]
    pub(crate) fn release_weight(&self) -> Option<f32> {
        self.release.as_ref().map(Release::weight)
    }

    /// Whether AI noise removal stopped because the processor could not keep up.
    pub fn ai_overloaded(&self) -> bool {
        self.denoiser.as_ref().is_some_and(Denoiser::overloaded)
    }

    /// Clean one frame of mono microphone audio using the playback `far` that played
    /// during it. Loud peaks are turned down, never clipped.
    pub fn process(
        &mut self,
        near: &[f32; FRAME],
        far: &[Stereo; FRAME],
        out: &mut [f32; FRAME],
    ) -> Result<(), PipelineError> {
        self.process_with_reference(near, far, out, true)
    }

    pub(crate) fn process_with_reference(
        &mut self,
        near: &[f32; FRAME],
        far: &[Stereo; FRAME],
        out: &mut [f32; FRAME],
        complete: bool,
    ) -> Result<(), PipelineError> {
        for (pair, sample) in self.far.as_chunks_mut::<2>().0.iter_mut().zip(far) {
            pair.copy_from_slice(sample);
        }
        let source = match &mut self.linear {
            Some(linear) => {
                if complete {
                    linear.process(near, &self.far, &mut self.mono)?;
                } else {
                    linear.process_unadapted(near, &self.far, &mut self.mono)?;
                }
                &self.mono
            }
            None => near,
        };
        for (pair, &sample) in self.near.as_chunks_mut::<2>().0.iter_mut().zip(source) {
            pair.fill(sample);
        }
        self.aec3.process(&self.near, &self.far, &mut self.clean)?;
        for (mono, [left, right]) in self.mono.iter_mut().zip(self.clean.as_chunks::<2>().0) {
            *mono = 0.5 * (left + right);
        }
        if let Some(release) = &mut self.release {
            release.process(&self.aec3, near, &self.near, &self.far, complete, &mut self.mono)?;
        }
        match &mut self.denoiser {
            Some(denoiser) => denoiser.process(&self.mono, out),
            None => out.copy_from_slice(&self.mono),
        }
        self.limiter.process(out);
        Ok(())
    }
}

/// Synthetic playback and microphone audio for tests of Adaptive's release.
#[cfg(test)]
pub(crate) mod signals {
    use echobridge_dsp::{FRAME, RATE, Stereo};

    /// 10 ms microphone frames, each with the playback that played during it.
    pub(crate) type Frames = Vec<([f32; FRAME], [Stereo; FRAME])>;

    /// Samples between a playback sample and its leak at the microphone.
    const LEAK_DELAY: usize = 45;
    /// The corner of the physically low-passed leak, in Hz.
    const LEAK_CUTOFF_HZ: f32 = 1_000.0;
    const LEAK_TAPS: usize = 255;
    /// Peak-to-peak level of the playback noise, in full scale.
    const MUSIC_LEVEL: f32 = 0.6;
    /// How much of the playback reaches the microphone, as a gain on the 0.5 and 0.3 mix.
    const LEAK_GAIN: f32 = 0.3;
    /// Peak-to-peak level of room noise that reaches the microphone through every frequency.
    const ROOM_NOISE: f32 = 0.00035;
    /// Peak-to-peak level of a voice at the microphone, in full scale.
    const VOICE_LEVEL: f32 = 0.1;
    /// The voice's lows are noise smoothed by a one-pole filter (about 760 Hz), scaled back to
    /// the white noise's level; its highs are what the smoothing leaves out.
    const VOICE_LOWS_SMOOTHING: f32 = 0.1;
    const VOICE_LOWS_GAIN: f32 = 4.4;
    const VOICE_HIGHS_GAIN: f32 = 0.2;

    struct Noise(u32);

    impl Noise {
        /// Uniform between -0.5 and 0.5.
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        }
    }

    /// Stereo noise whose level steps every 50 ms by up to 15 dB, like phrases of music.
    pub(crate) fn varying_music(seconds: usize) -> Vec<Stereo> {
        music(seconds, 7)
    }

    fn music(seconds: usize, seed: u32) -> Vec<Stereo> {
        let mut noise = Noise(seed);
        let mut gain = 1.0f32;
        (0..seconds * RATE as usize)
            .map(|t| {
                if t % (5 * FRAME) == 0 {
                    gain = 10f32.powf(-15.0 * (noise.next() + 0.5) / 20.0);
                }
                [MUSIC_LEVEL * gain * noise.next(), MUSIC_LEVEL * gain * noise.next()]
            })
            .collect()
    }

    /// How much of each playback channel reaches the microphone.
    type Mix = [f32; 2];

    /// Both channels leak, as through a headset's shared cable.
    const BOTH_CHANNELS: Mix = [0.5, 0.3];
    /// Only the left channel leaks, as when one earpiece sits against the microphone.
    const LEFT_ONLY: Mix = [0.5, 0.0];

    fn dry_leak(music: &[Stereo], mix: Mix) -> Vec<f32> {
        (0..music.len())
            .map(|t| {
                if t >= LEAK_DELAY {
                    LEAK_GAIN * (mix[0] * music[t - LEAK_DELAY][0] + mix[1] * music[t - LEAK_DELAY][1])
                } else {
                    0.0
                }
            })
            .collect()
    }

    fn frames(microphone: &[f32], music: &[Stereo]) -> Frames {
        let (near, far) = (microphone.as_chunks::<FRAME>().0, music.as_chunks::<FRAME>().0);
        near.iter().zip(far).map(|(near, far)| (*near, *far)).collect()
    }

    /// A leak through every frequency, as a wired headset's crosstalk.
    pub(crate) fn fullband_leak(music: &[Stereo]) -> Frames {
        frames(&dry_leak(music, BOTH_CHANNELS), music)
    }

    /// A leak low-passed at 1 kHz, with quiet
    /// room noise that has nothing to do with the playback.
    pub(crate) fn lowpass_leak(music: &[Stereo]) -> Frames {
        lowpass_microphone(music, BOTH_CHANNELS)
    }

    fn lowpass_microphone(music: &[Stereo], mix: Mix) -> Frames {
        let centre = (LEAK_TAPS / 2) as f32;
        let window = |n: f32| {
            let phase = std::f32::consts::TAU * n / (LEAK_TAPS - 1) as f32;
            0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos()
        };
        let cutoff = 2.0 * LEAK_CUTOFF_HZ / RATE as f32;
        let mut taps: Vec<f32> = (0..LEAK_TAPS)
            .map(|n| {
                let x = n as f32 - centre;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f32::consts::PI * cutoff * x).sin() / (std::f32::consts::PI * cutoff * x)
                };
                sinc * window(n as f32)
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|tap| *tap /= sum);
        let dry = dry_leak(music, mix);
        let mut noise = Noise(99);
        let mut swell = Noise(123_456);
        let mut room = 0.0f32;
        let microphone: Vec<f32> = (0..dry.len())
            .map(|t| {
                if t % (3 * FRAME) == 0 {
                    room = ROOM_NOISE * 10f32.powf(-12.0 * (swell.next() + 0.5) / 20.0);
                }
                let leak: f32 = taps.iter().enumerate().take(t + 1).map(|(k, tap)| tap * dry[t - k]).sum();
                leak + room * noise.next()
            })
            .collect();
        frames(&microphone, music)
    }

    /// Playback whose channels nearly cancel in a mono mix: the right channel is the left
    /// one inverted, plus half of a second, independent piece of music.
    pub(crate) fn wide_music(seconds: usize) -> Vec<Stereo> {
        let (left, other) = (music(seconds, 7), music(seconds, 31));
        left.iter().zip(&other).map(|(l, o)| [l[0], -l[0] + 0.5 * o[1]]).collect()
    }

    /// Playback whose mono mix is silence: the right channel is the left one inverted.
    pub(crate) fn antiphase_music(seconds: usize) -> Vec<Stereo> {
        music(seconds, 7).iter().map(|l| [l[0], -l[0]]).collect()
    }

    /// [`fullband_leak`] from the left channel only.
    pub(crate) fn left_only_fullband_leak(music: &[Stereo]) -> Frames {
        frames(&dry_leak(music, LEFT_ONLY), music)
    }

    /// [`lowpass_leak`] from the left channel only.
    pub(crate) fn left_only_lowpass_leak(music: &[Stereo]) -> Frames {
        lowpass_microphone(music, LEFT_ONLY)
    }

    /// The microphone of `before` until `frame`, then the microphone of `after`: the echo
    /// path changes while the same playback goes on.
    pub(crate) fn switch_leak(before: Frames, after: &Frames, frame: usize) -> Frames {
        let mut frames = before;
        for (kept, later) in frames.iter_mut().zip(after).skip(frame) {
            kept.0 = later.0;
        }
        frames
    }

    /// Adds a voice to the microphone: mostly lows, as speech is, with highs 15 dB under
    /// them. Its level steps every 100 ms by up to 9 dB, like syllables.
    pub(crate) fn add_voice(frames: &mut [([f32; FRAME], [Stereo; FRAME])], seed: u32) {
        let mut noise = Noise(seed);
        let mut gain = 1.0f32;
        let mut lows = 0.0f32;
        for (index, (near, _)) in frames.iter_mut().enumerate() {
            if index % 10 == 0 {
                gain = 10f32.powf(-9.0 * (noise.next() + 0.5) / 20.0);
            }
            for sample in near {
                let white = noise.next();
                lows += VOICE_LOWS_SMOOTHING * (white - lows);
                *sample += VOICE_LEVEL * gain * (VOICE_LOWS_GAIN * lows + VOICE_HIGHS_GAIN * (white - lows));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::signals::{
        Frames, add_voice, antiphase_music, fullband_leak, left_only_fullband_leak, left_only_lowpass_leak,
        lowpass_leak, switch_leak, varying_music, wide_music,
    };

    /// A stereo leak with a short electrical response, as in `echobridge-dsp`'s tests.
    fn leak_frames(seconds: usize) -> Vec<([f32; FRAME], [Stereo; FRAME])> {
        let mut state = 7u32;
        let mut noise = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let total = seconds * 48_000;
        let music: Vec<Stereo> = (0..total).map(|_| [0.2 * noise(), 0.2 * noise()]).collect();
        (0..total / FRAME)
            .map(|f| {
                let mut near = [0.0; FRAME];
                let mut far = [[0.0; 2]; FRAME];
                for i in 0..FRAME {
                    let t = f * FRAME + i;
                    far[i] = music[t];
                    if t >= 45 {
                        near[i] = 0.5 * music[t - 45][0] + 0.3 * music[t - 45][1];
                    }
                }
                (near, far)
            })
            .collect()
    }

    fn removal_db(options: Options) -> f64 {
        let mut pipeline = Pipeline::new(options).unwrap();
        let frames = leak_frames(8);
        let mut out = [0.0; FRAME];
        let (mut before, mut after) = (0.0, 0.0);
        for (index, (near, far)) in frames.iter().enumerate() {
            pipeline.process(near, far, &mut out).unwrap();
            if index >= 500 {
                before += near.iter().map(|&s| f64::from(s * s)).sum::<f64>();
                after += out.iter().map(|&s| f64::from(s * s)).sum::<f64>();
            }
        }
        10.0 * (before / after.max(1e-20)).log10()
    }

    #[test]
    fn clean_voice_removes_a_wired_leak() {
        let removed = removal_db(Options { echo: EchoMode::CleanVoice, ..Options::default() });
        assert!(removed > 30.0, "removed {removed:.1} dB");
    }

    #[test]
    fn switching_options_keeps_processing() {
        let mut pipeline = Pipeline::new(Options::default()).unwrap();
        let mut out = [0.0; FRAME];
        let (near, far) = &leak_frames(1)[0];
        for echo in [EchoMode::CleanVoice, EchoMode::Strong, EchoMode::Adaptive] {
            let options = Options { echo, ..pipeline.options() };
            pipeline.apply(Stages::prepare(options, Some(&pipeline.options())).unwrap());
            pipeline.process(near, far, &mut out).unwrap();
            assert_eq!(pipeline.options().echo, echo);
        }
    }

    #[test]
    fn output_is_limited_to_full_scale() {
        let mut pipeline = Pipeline::new(Options::default()).unwrap();
        let mut out = [0.0; FRAME];
        pipeline.process(&[1.0; FRAME], &[[0.0; 2]; FRAME], &mut out).unwrap();
        assert!(out.iter().all(|s| s.abs() <= 1.0));
    }

    /// Adaptive beside Strong, fed the same frames. The release policy is the production one.
    struct Comparison {
        adaptive: Pipeline,
        strong: Pipeline,
    }

    /// One frame of each pipeline, and how far the call hears Adaptive's second canceller.
    struct Pair {
        adaptive: [f32; FRAME],
        strong: [f32; FRAME],
        weight: f32,
    }

    impl Comparison {
        fn new(noise: NoiseRemoval) -> Self {
            let adaptive = Options { echo: EchoMode::Adaptive, noise, ..Options::default() };
            let strong = Options { echo: EchoMode::Strong, ..adaptive };
            Self { adaptive: Pipeline::new(adaptive).unwrap(), strong: Pipeline::new(strong).unwrap() }
        }

        fn step(&mut self, near: &[f32; FRAME], far: &[Stereo; FRAME]) -> Pair {
            let (mut adaptive, mut strong) = ([0.0; FRAME], [0.0; FRAME]);
            self.adaptive.process(near, far, &mut adaptive).unwrap();
            self.strong.process(near, far, &mut strong).unwrap();
            Pair { adaptive, strong, weight: self.adaptive.release_weight().unwrap() }
        }

        /// Run frames until the call hears the second canceller fully; report the frame index.
        fn run_until_released(&mut self, frames: &Frames) -> usize {
            frames
                .iter()
                .position(|(near, far)| self.step(near, far).weight == 1.0)
                .expect("the highs were never released")
        }

        fn restart_stream(&mut self) {
            self.adaptive.restart_stream();
            self.strong.restart_stream();
        }
    }

    fn same_bits(a: &[f32; FRAME], b: &[f32; FRAME]) -> bool {
        a.iter().zip(b).all(|(a, b)| a.to_bits() == b.to_bits())
    }

    #[test]
    fn adaptive_is_bit_exact_strong_while_it_warms_up() {
        let mut both = Comparison::new(NoiseRemoval::Standard);
        for (index, (near, far)) in leak_frames(2).iter().enumerate() {
            let pair = both.step(near, far);
            assert!(same_bits(&pair.adaptive, &pair.strong), "frame {index} differs from Strong");
            assert_eq!(pair.weight, 0.0);
        }
    }

    #[test]
    fn a_full_band_leak_keeps_strong_protection() {
        let frames = fullband_leak(&varying_music(25));
        let mut both = Comparison::new(NoiseRemoval::Off);
        for (index, (near, far)) in frames.iter().enumerate() {
            let pair = both.step(near, far);
            assert_eq!(pair.weight, 0.0, "released at frame {index} although the leak reaches the highs");
            assert!(same_bits(&pair.adaptive, &pair.strong), "frame {index} differs from Strong");
        }
    }

    #[test]
    fn a_low_passed_leak_with_independent_highs_releases_and_stays_released() {
        let frames = lowpass_leak(&varying_music(30));
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&frames);
        assert!(released >= 500, "released at frame {released}, before the dwell");
        for (near, far) in &frames[released..] {
            let pair = both.step(near, far);
            assert_eq!(pair.weight, 1.0, "the released highs were taken away again");
            assert!(pair.adaptive.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
    }

    #[test]
    fn restarts_resets_and_new_stages_return_to_strong_at_once() {
        let frames = lowpass_leak(&varying_music(30));
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&frames);
        let mut rest = frames[released..].iter();
        both.restart_stream();
        assert_eq!(both.adaptive.release_weight(), Some(0.0));
        for (near, far) in rest.by_ref().take(200) {
            let pair = both.step(near, far);
            assert!(same_bits(&pair.adaptive, &pair.strong));
            assert_eq!(pair.weight, 0.0);
        }
        both.adaptive.reset();
        assert_eq!(both.adaptive.release_weight(), Some(0.0));
        let options = both.adaptive.options();
        both.adaptive.apply(Stages::prepare(options, Some(&options)).unwrap());
        assert_eq!(both.adaptive.release_weight(), Some(0.0));
        let (near, far) = rest.next().unwrap();
        let mut out = [0.0; FRAME];
        both.adaptive.process(near, far, &mut out).unwrap();
        assert_eq!(both.adaptive.release_weight(), Some(0.0));
    }

    #[test]
    fn a_missing_reference_frame_returns_to_strong_within_that_frame() {
        let frames = lowpass_leak(&varying_music(30));
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&frames);
        let (near, _) = &frames[released + 1];
        let mut out = [0.0; FRAME];
        both.adaptive.process_with_reference(near, &[[0.0; 2]; FRAME], &mut out, false).unwrap();
        assert_eq!(both.adaptive.release_weight(), Some(0.0));
    }

    #[test]
    fn ai_noise_removal_runs_on_the_same_stream_through_release_and_restart() {
        let frames = lowpass_leak(&varying_music(14));
        let mut both = Comparison::new(NoiseRemoval::Ai);
        let released = both.run_until_released(&frames);
        for (index, (near, far)) in frames[released + 1..].iter().enumerate() {
            if index == 100 {
                both.restart_stream();
            }
            let pair = both.step(near, far);
            assert!(same_bits(&pair.adaptive, &pair.strong), "AI output differs from Strong at {index}");
        }
        assert!(!both.adaptive.ai_overloaded());
    }

    #[test]
    fn the_released_path_reaches_the_call_while_the_user_speaks() {
        let frames = lowpass_leak(&varying_music(30));
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&frames);
        let mut seed = 5u32;
        let mut heard_difference = false;
        for (near, far) in &frames[released + 51..released + 81] {
            let mut near = *near;
            for sample in &mut near {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *sample += 0.1 * ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
            }
            let pair = both.step(&near, far);
            heard_difference |= pair.weight > 0.0 && !same_bits(&pair.adaptive, &pair.strong);
            assert!(pair.adaptive.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert!(heard_difference, "the released canceller never differed from Strong during a voice");
    }

    /// Adds a voice to the `count` frames after `released`, and returns them.
    fn voiced_after(frames: &mut Frames, released: usize, count: usize) -> &[([f32; FRAME], [Stereo; FRAME])] {
        let voiced = &mut frames[released + 1..released + 1 + count];
        add_voice(voiced, 5);
        voiced
    }

    /// The frames after a full-band leak replaces a low-passed one, with a voice from the
    /// release on. Returns the index in the returned frames where the leak changes.
    fn leak_change_under_voice(before: Frames, after: &Frames, released: usize, count: usize) -> (Frames, usize) {
        let changes_at = 101;
        let mut frames = switch_leak(before, after, released + 1 + changes_at);
        voiced_after(&mut frames, released, count);
        (frames, changes_at)
    }

    /// Frames from the release on until the call stops hearing the released path in full;
    /// `None` if it never stops.
    fn frames_until_strong(both: &mut Comparison, frames: &[([f32; FRAME], [Stereo; FRAME])]) -> Option<usize> {
        frames.iter().position(|(near, far)| both.step(near, far).weight < 1.0)
    }

    #[test]
    fn restored_voice_highs_over_playback_stay_released() {
        let mut frames = lowpass_leak(&varying_music(30));
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&frames);
        let voiced = voiced_after(&mut frames, released, 500);
        let mut differing = 0;
        for (index, (near, far)) in voiced.iter().enumerate() {
            let pair = both.step(near, far);
            assert_eq!(pair.weight, 1.0, "the voice lost the released highs {index} frames in");
            differing += usize::from(!same_bits(&pair.adaptive, &pair.strong));
        }
        assert!(differing > 450, "the released highs differed from Strong in only {differing} voiced frames");
    }

    #[test]
    fn a_new_full_band_leak_under_a_voice_falls_back_within_a_second() {
        let music = varying_music(30);
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&lowpass_leak(&music));
        let (frames, changes_at) = leak_change_under_voice(lowpass_leak(&music), &fullband_leak(&music), released, 400);
        let voiced = &frames[released + 1..released + 401];
        let kept = frames_until_strong(&mut both, voiced);
        let kept = kept.expect("the released highs stayed with a full-band leak under a voice");
        assert!(kept >= changes_at, "fell back before the leak changed");
        let frames_after = kept - changes_at;
        assert!(frames_after <= 100, "fell back {frames_after} frames after the leak changed");
    }

    #[test]
    fn playback_whose_channels_cancel_in_a_mono_mix_still_keeps_a_one_channel_leak_protected() {
        let frames = left_only_fullband_leak(&wide_music(25));
        let mut both = Comparison::new(NoiseRemoval::Off);
        for (index, (near, far)) in frames.iter().enumerate() {
            let pair = both.step(near, far);
            assert_eq!(pair.weight, 0.0, "released at frame {index} although the left channel leaks highs");
            assert!(same_bits(&pair.adaptive, &pair.strong), "frame {index} differs from Strong");
        }
    }

    #[test]
    fn a_silent_mono_mix_does_not_disable_the_guard_under_a_voice() {
        let music = antiphase_music(30);
        let mut both = Comparison::new(NoiseRemoval::Off);
        let released = both.run_until_released(&left_only_lowpass_leak(&music));
        let (frames, changes_at) =
            leak_change_under_voice(left_only_lowpass_leak(&music), &left_only_fullband_leak(&music), released, 400);
        let voiced = &frames[released + 1..released + 401];
        let kept = frames_until_strong(&mut both, voiced);
        let kept = kept.expect("an antiphase playback hid a full-band leak");
        assert!(kept >= changes_at, "fell back before the leak changed");
        let frames_after = kept - changes_at;
        assert!(frames_after <= 100, "fell back {frames_after} frames after the leak changed");
    }
}
