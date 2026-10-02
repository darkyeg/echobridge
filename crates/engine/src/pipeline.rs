//! The per-frame processing chain, in Chrome's order: leak removal, then noise removal.

use echobridge_aec3::EchoProcessor;
use echobridge_denoise::Denoiser;
use echobridge_dsp::limiter::Limiter;
use echobridge_dsp::linear::{LinearCanceller, LinearConfig};
use echobridge_dsp::{FRAME, Stereo};

use crate::options::{EchoMode, NoiseRemoval, Options};

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
        Ok(Self {
            options,
            aec3: EchoProcessor::new(&options.aec3_config())?,
            linear: (options.echo == EchoMode::CleanVoice).then(|| LinearCanceller::new(LinearConfig::default())),
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
        if let Some(denoiser) = stages.denoiser {
            self.denoiser = denoiser;
        }
    }

    /// Forget learned echo paths, as after a gap in the audio. The AI model keeps its
    /// state: it holds only the last 30 ms.
    pub fn reset(&mut self) {
        self.aec3.reset();
        if let Some(linear) = &mut self.linear {
            linear.reset();
        }
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
        for (pair, sample) in self.far.as_chunks_mut::<2>().0.iter_mut().zip(far) {
            pair.copy_from_slice(sample);
        }
        let source = match &mut self.linear {
            Some(linear) => {
                linear.process(near, &self.far, &mut self.mono)?;
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
        match &mut self.denoiser {
            Some(denoiser) => denoiser.process(&self.mono, out),
            None => out.copy_from_slice(&self.mono),
        }
        self.limiter.process(out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
