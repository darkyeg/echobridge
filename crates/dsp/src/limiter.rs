//! Keeping the clean microphone within full scale without clipping it.
//!
//! Subtracting the leak from a microphone that clipped, or removing noise from a loud
//! voice, can produce samples beyond full scale (measured: up to 1.17 with the microphone
//! at +22 dB). Cutting them off is heard as crackle. The limiter lowers the gain for loud
//! peaks instead and lets it recover slowly; audio that never comes close passes through
//! unchanged, bit for bit.

/// The highest sample level let through.
const CEILING: f32 = 0.98;
/// Gain recovery per sample: about 20 dB per second at 48 kHz.
const RELEASE: f32 = 1.000_048;

#[derive(Debug, Clone)]
pub struct Limiter {
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Limiter {
    /// The current gain, 1 when nothing is being limited.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Limit one frame in place. The gain falls at once to fit the frame's peak and rises
    /// slowly afterwards; anything still above the ceiling is clamped.
    pub fn process(&mut self, samples: &mut [f32]) {
        let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        let target = if peak > CEILING { CEILING / peak } else { 1.0 };
        if target >= 1.0 && self.gain >= 1.0 {
            return;
        }
        for sample in samples.iter_mut() {
            self.gain = if self.gain > target { target } else { (self.gain * RELEASE).min(target) };
            *sample = (*sample * self.gain).clamp(-CEILING, CEILING);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_audio_passes_unchanged() {
        let mut limiter = Limiter::default();
        let original: Vec<f32> = (0..480).map(|i| 0.5 * (i as f32 * 0.05).sin()).collect();
        let mut frame = original.clone();
        limiter.process(&mut frame);
        assert_eq!(frame, original);
    }

    #[test]
    fn loud_peaks_are_scaled_not_cut() {
        let mut limiter = Limiter::default();
        let mut frame: Vec<f32> = (0..480).map(|i| 1.17 * (i as f32 * 0.05).sin()).collect();
        limiter.process(&mut frame);
        let peak = frame.iter().fold(0.0f32, |p, s| p.max(s.abs()));
        assert!(peak <= CEILING && peak > 0.95, "{peak}");
        // Scaled, so the shape is kept: no flat tops.
        let flat = frame.windows(3).filter(|w| w.iter().all(|s| (s.abs() - CEILING).abs() < 1e-6)).count();
        assert_eq!(flat, 0);
    }

    #[test]
    fn gain_recovers_after_a_peak() {
        let mut limiter = Limiter::default();
        limiter.process(&mut vec![1.5; 480]);
        assert!(limiter.gain() < 0.7);
        for _ in 0..300 {
            limiter.process(&mut vec![0.1; 480]); // 3 s of quiet audio
        }
        assert_eq!(limiter.gain(), 1.0);
    }
}
