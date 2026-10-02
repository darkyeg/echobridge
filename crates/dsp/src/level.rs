//! Signal levels for meters and measurements.

/// The quietest level reported, in dBFS; anything below reads as silence.
pub const FLOOR_DBFS: f32 = -90.0;

/// RMS level of `samples` in dBFS, at least [`FLOOR_DBFS`].
pub fn dbfs(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return FLOOR_DBFS;
    }
    let power = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum::<f64>() / samples.len() as f64;
    (power_db(power) as f32).max(FLOOR_DBFS)
}

/// Mean power in dB, with a floor at -150 dB so silence stays finite.
pub fn power_db(power: f64) -> f64 {
    10.0 * power.max(1e-15).log10()
}

/// Whether any sample is close enough to full scale to be clipping.
pub fn clipping(samples: &[f32]) -> bool {
    samples.iter().any(|s| s.abs() > 0.98)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_square_is_zero_dbfs() {
        assert!(dbfs(&[1.0, -1.0, 1.0, -1.0]).abs() < 1e-6);
    }

    #[test]
    fn silence_reads_as_the_floor() {
        assert_eq!(dbfs(&[0.0; 480]), FLOOR_DBFS);
        assert_eq!(dbfs(&[]), FLOOR_DBFS);
    }

    #[test]
    fn half_amplitude_is_minus_six_db() {
        assert!((dbfs(&[0.5, -0.5]) + 6.0206).abs() < 1e-3);
    }
}
