//! Live meter levels that the window can read at its own frame rate, without locks or
//! waiting for the statistics snapshot.

use std::sync::atomic::{AtomicU32, Ordering};

use echobridge_dsp::level::FLOOR_DBFS;

/// Meter levels in dBFS: frame levels with a 20 dB/s release.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Levels {
    pub microphone_dbfs: f32,
    pub playback_dbfs: f32,
    pub output_dbfs: f32,
}

impl Default for Levels {
    fn default() -> Self {
        Self { microphone_dbfs: FLOOR_DBFS, playback_dbfs: FLOOR_DBFS, output_dbfs: FLOOR_DBFS }
    }
}

/// The latest [`Levels`], written by the processing thread every 10 ms frame.
#[derive(Debug)]
pub struct Meters([AtomicU32; 3]);

impl Default for Meters {
    fn default() -> Self {
        let floor = || AtomicU32::new(FLOOR_DBFS.to_bits());
        Self([floor(), floor(), floor()])
    }
}

impl Meters {
    pub fn levels(&self) -> Levels {
        let [microphone, playback, output] =
            self.0.each_ref().map(|level| f32::from_bits(level.load(Ordering::Relaxed)));
        Levels { microphone_dbfs: microphone, playback_dbfs: playback, output_dbfs: output }
    }

    pub(crate) fn store(&self, levels: Levels) {
        let values = [levels.microphone_dbfs, levels.playback_dbfs, levels.output_dbfs];
        for (level, value) in self.0.iter().zip(values) {
            level.store(value.to_bits(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_silent_and_returns_what_was_stored() {
        let meters = Meters::default();
        assert_eq!(meters.levels(), Levels::default());
        let levels = Levels { microphone_dbfs: -20.5, playback_dbfs: -31.0, output_dbfs: -64.25 };
        meters.store(levels);
        assert_eq!(meters.levels(), levels);
    }
}
