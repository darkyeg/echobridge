//! A small, self-centring buffer between two audio clocks.
//!
//! The processed microphone is produced on the microphone's clock and consumed on the
//! output device's clock. The two differ by a few parts per million, which would slowly
//! add delay or run dry. When the fill drifts beyond scheduling jitter, a read takes
//! slightly more or fewer samples than asked for, resampled to the requested count, so the
//! buffer returns to its target. Within the jitter band audio passes through bit-exact:
//! resampling interpolates, which dulls high frequencies, so it is kept for real drift.

use std::collections::VecDeque;

/// Fill the buffer aims for, in seconds: enough to ride out scheduling jitter.
const TARGET: f64 = 0.01;
/// Above this fill (seconds) the buffer is cut back to its reserve; delay must never pile up.
const MAXIMUM: f64 = 0.15;
const TRIM_TO: f64 = 0.02;
/// Fill errors within this many seconds are jitter and leave the audio untouched.
const DEADBAND: f64 = 0.01;

/// Mono samples waiting for the output device.
#[derive(Debug)]
pub struct ElasticBuffer {
    data: VecDeque<f32>,
    rate: f64,
    target: usize,
    maximum: usize,
    trim_to: usize,
    deadband: usize,
    /// Reads that found too little audio and were completed with silence.
    pub underflows: u64,
    /// Times the buffer was cut back because audio piled up.
    pub trims: u64,
    /// Temporary silence reserves added before a reference wait.
    pub padding_events: u64,
}

impl ElasticBuffer {
    /// A buffer at `rate` Hz, starting at its target fill of silence.
    pub fn new(rate: u32) -> Self {
        let samples = |seconds: f64| (f64::from(rate) * seconds) as usize;
        Self {
            data: std::iter::repeat_n(0.0, samples(TARGET)).collect(),
            rate: f64::from(rate),
            target: samples(TARGET),
            maximum: samples(MAXIMUM),
            trim_to: samples(TRIM_TO),
            deadband: samples(DEADBAND),
            underflows: 0,
            trims: 0,
            padding_events: 0,
        }
    }

    /// Aim for `seconds` of audio, at least the default 10 ms. A higher target is reached at
    /// once with silence, so the extra margin is there for the next late frame.
    pub fn set_target(&mut self, seconds: f64) {
        // Leave one normal frame below the queue limit, including at the largest shift.
        let target = ((self.rate * seconds.max(TARGET)) as usize).min(self.maximum - (self.rate * TARGET) as usize);
        if target > self.target {
            let padding = (target - self.target)
                .max(target.saturating_sub(self.data.len()))
                .min(self.maximum.saturating_sub(self.data.len()));
            self.data.extend(std::iter::repeat_n(0.0, padding));
        } else if target < self.target && self.data.len() > target + self.deadband {
            // A smaller alignment shift must not leave a minute of excess latency while
            // the clock-drift correction removes only one sample per callback.
            self.data.drain(..self.data.len() - target - self.deadband);
            self.trims += 1;
        }
        self.target = target;
    }

    pub fn target_seconds(&self) -> f64 {
        self.target as f64 / self.rate
    }

    /// Reserve one upcoming wait without raising the steady clock-drift target. Once
    /// consumed, the silence does not leave a permanent queue of added latency.
    pub fn reserve(&mut self, seconds: f64) -> usize {
        let reserve = ((self.rate * seconds.max(0.0)) as usize).min(self.maximum - (self.rate * TARGET) as usize);
        if self.data.len() < reserve {
            let added = reserve - self.data.len();
            self.data.resize(reserve, 0.0);
            self.padding_events += 1;
            added
        } else {
            0
        }
    }

    /// Remove unused tail silence when that wait ends early. Call before pushing the
    /// processed frame: only the reserve can then occupy the tail, never new speech.
    pub fn release_reserve(&mut self, added: usize) {
        // The next voice frame replenishes the queue. Keeping a target-sized tail here
        // would leave reserved silence between consecutive voice frames.
        let unused = added.min(self.data.len());
        self.data.truncate(self.data.len() - unused);
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn push(&mut self, samples: &[f32]) {
        self.data.extend(samples);
        if self.data.len() > self.maximum {
            let excess = self.data.len() - self.trim_to.max(self.target);
            self.data.drain(..excess);
            self.trims += 1;
        }
    }

    /// Fill `out` completely, stretching or squeezing by at most 1 in 300 samples.
    pub fn pull(&mut self, out: &mut [f32]) {
        let count = out.len();
        if count == 0 {
            return;
        }
        let error = self.data.len() as i64 - count as i64 - self.target as i64;
        let excess = error.abs() - self.deadband as i64;
        let adjustment = if excess > 0 {
            let limit = (count / 300).max(1) as i64;
            (excess / 100 + 1).min(limit) * error.signum()
        } else {
            0
        };
        let take = (count as i64 + adjustment).max(1) as usize;
        if self.data.len() < take {
            self.underflows += 1;
            let available = self.data.len().min(count);
            for (o, s) in out.iter_mut().zip(self.data.drain(..available)) {
                *o = s;
            }
            out[available..].fill(0.0);
            // Start again from the target fill: refilling by resampling takes seconds, during
            // which every late frame would be another gap. One longer gap is heard as less.
            self.data.extend(std::iter::repeat_n(0.0, self.target));
            return;
        }
        if take == count {
            for (o, s) in out.iter_mut().zip(self.data.drain(..count)) {
                *o = s;
            }
            return;
        }
        let (front, back) = self.data.as_slices();
        let sample = |i: usize| if i < front.len() { front[i] } else { back[i - front.len()] };
        // Linear interpolation of `take` samples onto `count` (NumPy's interp).
        let step = take as f64 / count as f64;
        for (i, o) in out.iter_mut().enumerate() {
            let position = i as f64 * step;
            let index = position as usize;
            let fraction = (position - index as f64) as f32;
            let a = sample(index);
            let b = if index + 1 < take { sample(index + 1) } else { a };
            *o = a + (b - a) * fraction;
        }
        self.data.drain(..take);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_flow_keeps_the_target_fill() {
        let mut buffer = ElasticBuffer::new(48_000);
        let mut out = [0.0; 480];
        for _ in 0..1000 {
            buffer.push(&[0.1; 480]);
            buffer.pull(&mut out);
        }
        assert_eq!(buffer.underflows, 0);
        assert!((buffer.len() as i64 - 480).abs() < 480);
    }

    #[test]
    fn jitter_passes_audio_through_unchanged() {
        let mut buffer = ElasticBuffer::new(48_000);
        let signal: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.7).sin()).collect();
        let mut output = Vec::new();
        let mut out = [0.0; 480];
        // The output reads in bursts, up to one block late or early, at the same mean rate.
        for (i, block) in signal.chunks(480).enumerate() {
            buffer.push(block);
            for _ in 0..[1, 0, 2, 1, 1, 0, 1, 2, 1, 1][i % 10] {
                buffer.pull(&mut out);
                output.extend_from_slice(&out);
            }
        }
        let delay = 480; // the target fill of silence at the start
        assert_eq!(&output[delay..], &signal[..output.len() - delay]);
    }

    #[test]
    fn a_faster_producer_does_not_add_delay() {
        let mut buffer = ElasticBuffer::new(48_000);
        let mut out = [0.0; 480];
        // The producer runs 0.2 % fast: 481 samples for every 480 read.
        for _ in 0..20_000 {
            buffer.push(&[0.1; 481]);
            buffer.pull(&mut out);
        }
        assert!(buffer.len() < 1440, "fill {}", buffer.len());
        assert_eq!(buffer.trims, 0);
    }

    #[test]
    fn running_dry_pads_with_silence_and_counts_it() {
        let mut buffer = ElasticBuffer::new(48_000);
        let mut out = [1.0; 2000];
        buffer.pull(&mut out);
        assert_eq!(buffer.underflows, 1);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(buffer.len(), 480, "refilled to the target");
    }

    #[test]
    fn a_late_frame_after_a_gap_does_not_cause_another() {
        let mut buffer = ElasticBuffer::new(48_000);
        let mut out = [0.0; 480];
        buffer.pull(&mut out);
        buffer.pull(&mut out); // ran dry once
        assert_eq!(buffer.underflows, 1);
        // Frames now arrive one pull late; the refilled margin absorbs it.
        for _ in 0..100 {
            buffer.pull(&mut out);
            buffer.push(&[0.5; 480]);
        }
        assert_eq!(buffer.underflows, 1);
    }

    #[test]
    fn a_higher_target_is_reached_at_once() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.set_target(0.02);
        assert_eq!(buffer.len(), 960);
        assert!((buffer.target_seconds() - 0.02).abs() < 1e-9);
        buffer.set_target(0.0);
        assert!((buffer.target_seconds() - 0.01).abs() < 1e-9, "never below the default");
    }

    #[test]
    fn a_long_reference_delay_keeps_its_margin_after_trimming_and_underflow() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.set_target(0.13);
        assert_eq!(buffer.len(), 6240, "reserve the learned reference delay");
        buffer.push(&vec![0.5; 24_000]);
        assert_eq!(buffer.trims, 1);
        assert_eq!(buffer.len(), 6240, "trimming must preserve the reference reserve");
        buffer.pull(&mut [0.0; 8000]);
        assert_eq!(buffer.len(), 6240, "underflow must restore the reference reserve");
        assert!(buffer.len() < 7200, "the queue remains bounded");
    }

    #[test]
    fn reducing_a_reference_delay_removes_excess_latency_immediately() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.set_target(0.133);
        buffer.set_target(0.01);
        assert!(buffer.len() <= 960);
        assert_eq!(buffer.trims, 1);
        buffer.set_target(f64::INFINITY);
        assert!((buffer.target_seconds() - 0.14).abs() < 1e-9);
        assert!(buffer.len() <= 7200);
    }

    #[test]
    fn a_temporary_reference_wait_does_not_raise_steady_latency() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.set_target(0.02);
        buffer.reserve(0.103);
        assert_eq!(buffer.len(), 4944);
        assert_eq!(buffer.padding_events, 1);
        assert!((buffer.target_seconds() - 0.02).abs() < 1e-9);
        for _ in 0..8 {
            buffer.pull(&mut [0.0; 480]);
        }
        assert!(buffer.len() <= 1200, "the 80 ms wait consumed the temporary reserve");
        assert_eq!(buffer.underflows, 0);
        buffer.push(&[0.5; 480]);
        buffer.reserve(0.033);
        buffer.pull(&mut [0.0; 480]);
        let settled = buffer.padding_events;
        for _ in 0..200 {
            buffer.push(&[0.5; 480]);
            buffer.reserve(0.033);
            buffer.pull(&mut [0.0; 480]);
        }
        assert!(buffer.len() <= 1440, "steady fill stayed near 20 ms");
        assert_eq!(buffer.padding_events, settled, "steady flow must not keep inserting silence");
        buffer.pull(&mut [0.0; 8000]);
        assert_eq!(buffer.len(), 960, "underflow restores the steady target");
    }

    #[test]
    fn an_early_reference_arrival_reclaims_only_unused_silence() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.push(&[0.5; 480]);
        let added = buffer.reserve(0.103);
        buffer.release_reserve(added);
        assert!(buffer.len() <= 960, "an early arrival must not leave a large backlog");
        let mut out = [0.0; 480];
        buffer.pull(&mut out);
        assert_eq!(out, [0.0; 480]);
        buffer.push(&[0.75; 480]);
        buffer.pull(&mut out);
        assert_eq!(out, [0.5; 480], "queued voice preceding the reserve stays intact");
        buffer.push(&[0.25; 480]);
        buffer.pull(&mut out);
        assert_eq!(out, [0.75; 480], "voice added after reclaiming is preserved too");
    }

    #[test]
    fn releasing_a_reserve_after_rendering_keeps_consecutive_voice_frames() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.set_target(0.02);
        buffer.push(&[0.5; 960]);
        buffer.pull(&mut [0.0; 960]);
        let added = buffer.reserve(0.033);
        let mut out = [0.0; 480];
        buffer.pull(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.5));
        buffer.release_reserve(added);
        buffer.push(&[0.75; 480]);
        buffer.pull(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.5), "the remaining older voice stays intact");
        buffer.push(&[0.25; 480]);
        buffer.pull(&mut out);
        assert!(out.iter().all(|&sample| sample == 0.75), "unused reserve must not mute the next voice frame");
    }

    #[test]
    fn piled_up_audio_is_trimmed() {
        let mut buffer = ElasticBuffer::new(48_000);
        buffer.push(&vec![0.0; 48_000]);
        assert_eq!(buffer.trims, 1);
        assert_eq!(buffer.len(), 960);
    }
}
