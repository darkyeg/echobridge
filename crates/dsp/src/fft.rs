//! Real FFTs with NumPy's conventions, so measurements match the Python prototypes.

use std::sync::Arc;

use realfft::num_complex::Complex64;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// A forward and inverse real FFT of one size, with their scratch buffers.
pub struct RealFft {
    size: usize,
    forward: Arc<dyn RealToComplex<f64>>,
    inverse: Arc<dyn ComplexToReal<f64>>,
    time: Vec<f64>,
    spectrum: Vec<Complex64>,
    forward_scratch: Vec<Complex64>,
    inverse_scratch: Vec<Complex64>,
}

impl std::fmt::Debug for RealFft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealFft").field("size", &self.size).finish()
    }
}

impl RealFft {
    pub fn new(size: usize) -> Self {
        let mut planner = RealFftPlanner::new();
        let forward = planner.plan_fft_forward(size);
        let inverse = planner.plan_fft_inverse(size);
        Self {
            size,
            time: forward.make_input_vec(),
            spectrum: forward.make_output_vec(),
            forward_scratch: forward.make_scratch_vec(),
            inverse_scratch: inverse.make_scratch_vec(),
            forward,
            inverse,
        }
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Number of frequency bins: `size / 2 + 1`.
    pub fn bins(&self) -> usize {
        self.size / 2 + 1
    }

    /// `numpy.fft.rfft(input, size)`: shorter input is zero-padded.
    pub fn forward(&mut self, input: &[f64], output: &mut [Complex64]) {
        let length = input.len().min(self.size);
        self.time[..length].copy_from_slice(&input[..length]);
        self.time[length..].fill(0.0);
        self.forward
            .process_with_scratch(&mut self.time, output, &mut self.forward_scratch)
            .expect("buffer sizes are fixed at construction");
    }

    /// `numpy.fft.irfft(spectrum, size)`: scaled by `1 / size`, and the imaginary parts of
    /// the DC and Nyquist bins are ignored as NumPy ignores them.
    pub fn inverse(&mut self, spectrum: &[Complex64], output: &mut [f64]) {
        self.spectrum.copy_from_slice(spectrum);
        self.spectrum[0].im = 0.0;
        if self.size.is_multiple_of(2) {
            let last = self.spectrum.len() - 1;
            self.spectrum[last].im = 0.0;
        }
        self.inverse
            .process_with_scratch(&mut self.spectrum, output, &mut self.inverse_scratch)
            .expect("buffer sizes are fixed at construction and edge bins are real");
        let scale = 1.0 / self.size as f64;
        output.iter_mut().for_each(|x| *x *= scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_undoes_forward() {
        let mut fft = RealFft::new(480);
        let input: Vec<f64> = (0..480).map(|i| ((i * 7919) % 101) as f64 - 50.0).collect();
        let mut spectrum = vec![Complex64::default(); fft.bins()];
        let mut output = vec![0.0; 480];
        fft.forward(&input, &mut spectrum);
        fft.inverse(&spectrum, &mut output);
        for (a, b) in input.iter().zip(&output) {
            assert!((a - b).abs() < 1e-9);
        }
    }

    #[test]
    fn forward_zero_pads_short_input() {
        let mut fft = RealFft::new(8);
        let mut spectrum = vec![Complex64::default(); fft.bins()];
        fft.forward(&[1.0], &mut spectrum);
        assert!(spectrum.iter().all(|c| (c.re - 1.0).abs() < 1e-12 && c.im.abs() < 1e-12));
    }
}
