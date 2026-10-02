//! Turning device packets of any size into fixed-size, time-stamped blocks.

use crate::{CaptureBlock, RATE};

/// Packets further than this (seconds) from where the waiting samples end do not continue
/// them: the loopback resumed after silence, or capture glitched.
const CONTINUITY: f64 = 0.002;

/// Collects packets and emits blocks of exactly `frames` frames. Partial blocks are never
/// padded; a gap discards them and marks the next block as a discontinuity.
#[derive(Debug)]
pub struct BlockAssembler {
    channels: usize,
    frames: usize,
    pending: Vec<f32>,
    pending_time: f64,
    discontinuity: bool,
    /// Frames thrown away at gaps.
    pub discarded_frames: u64,
}

impl BlockAssembler {
    pub fn new(channels: usize, frames: usize) -> Self {
        Self {
            channels,
            frames,
            pending: Vec::with_capacity(2 * frames * channels),
            pending_time: 0.0,
            discontinuity: false,
            discarded_frames: 0,
        }
    }

    /// Add a packet whose first frame was captured at `time`. `samples` is `None` for a
    /// packet the device marked silent. Complete blocks go to `emit` with `gain`.
    pub fn push(
        &mut self,
        samples: Option<&[f32]>,
        frames: usize,
        time: f64,
        discontinuity: bool,
        gain: f32,
        emit: &mut dyn FnMut(CaptureBlock<'_>),
    ) {
        let waiting = self.pending.len() / self.channels;
        if waiting > 0 && (time - (self.pending_time + waiting as f64 / f64::from(RATE))).abs() > CONTINUITY {
            self.discarded_frames += waiting as u64;
            self.pending.clear();
            self.discontinuity = true;
        }
        if self.pending.is_empty() {
            self.pending_time = time;
        }
        self.discontinuity |= discontinuity;
        let values = frames * self.channels;
        match samples {
            Some(samples) => self.pending.extend_from_slice(&samples[..values]),
            None => self.pending.resize(self.pending.len() + values, 0.0),
        }
        let block = self.frames * self.channels;
        let mut start = 0;
        while self.pending.len() - start >= block {
            emit(CaptureBlock {
                samples: &self.pending[start..start + block],
                channels: self.channels,
                time: self.pending_time,
                discontinuity: self.discontinuity,
                gain,
            });
            start += block;
            self.pending_time += self.frames as f64 / f64::from(RATE);
            self.discontinuity = false;
        }
        self.pending.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(assembler: &mut BlockAssembler, packets: &[(usize, f64)]) -> Vec<(f64, bool, Vec<f32>)> {
        let mut blocks = Vec::new();
        let mut counter = 0.0;
        for &(frames, time) in packets {
            let samples: Vec<f32> = (0..frames)
                .map(|_| {
                    counter += 1.0;
                    counter
                })
                .collect();
            assembler.push(Some(&samples), frames, time, false, 1.0, &mut |b| {
                blocks.push((b.time, b.discontinuity, b.samples.to_vec()));
            });
        }
        blocks
    }

    #[test]
    fn regroups_packets_into_blocks_with_exact_times() {
        let mut assembler = BlockAssembler::new(1, 480);
        let packets: Vec<_> = (0..10).map(|i| (448, i as f64 * 448.0 / 48_000.0)).collect();
        let blocks = collect(&mut assembler, &packets);
        assert_eq!(blocks.len(), 9);
        for (index, (time, discontinuity, samples)) in blocks.iter().enumerate() {
            assert!((time - index as f64 * 0.01).abs() < 1e-9);
            assert!(!discontinuity);
            assert_eq!(samples[0], (index * 480 + 1) as f32);
        }
    }

    #[test]
    fn a_gap_discards_the_partial_block_and_marks_the_next() {
        let mut assembler = BlockAssembler::new(1, 480);
        let blocks = collect(&mut assembler, &[(300, 0.0), (480, 1.0)]);
        assert_eq!(assembler.discarded_frames, 300);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, 1.0);
        assert!(blocks[0].1);
    }

    #[test]
    fn silent_packets_become_zeros() {
        let mut assembler = BlockAssembler::new(2, 4);
        let mut out = Vec::new();
        assembler.push(None, 4, 0.0, false, 0.5, &mut |b| out.push((b.samples.to_vec(), b.gain)));
        assert_eq!(out, vec![(vec![0.0; 8], 0.5)]);
    }
}
