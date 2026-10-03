//! Aggregate audio-health counters without losing failures between UI refreshes.

use std::fmt;
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub frames: u64,
    pub incomplete_reference: u64,
    pub gaps: u64,
    pub trims: u64,
    pub dropped: u64,
    pub retrained: u64,
    pub stream_resets: u64,
    pub reference_discontinuities: u64,
}

impl Counters {
    fn since(self, before: Self) -> Self {
        Self {
            frames: self.frames.saturating_sub(before.frames),
            incomplete_reference: self.incomplete_reference.saturating_sub(before.incomplete_reference),
            gaps: self.gaps.saturating_sub(before.gaps),
            trims: self.trims.saturating_sub(before.trims),
            dropped: self.dropped.saturating_sub(before.dropped),
            retrained: self.retrained.saturating_sub(before.retrained),
            stream_resets: self.stream_resets.saturating_sub(before.stream_resets),
            reference_discontinuities: self.reference_discontinuities.saturating_sub(before.reference_discontinuities),
        }
    }

    fn decreased(self, before: Self) -> bool {
        self.frames < before.frames
            || self.incomplete_reference < before.incomplete_reference
            || self.gaps < before.gaps
            || self.trims < before.trims
            || self.dropped < before.dropped
            || self.retrained < before.retrained
            || self.stream_resets < before.stream_resets
            || self.reference_discontinuities < before.reference_discontinuities
    }

    fn problems(self) -> bool {
        self.incomplete_reference > 0 || self.gaps > 0 || self.trims > 0 || self.dropped > 0
            || self.stream_resets > 0 || self.reference_discontinuities > 0
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Health {
    pub counters: Counters,
    pub processing: bool,
    pub coverage: f32,
    pub reference_shift_ms: f32,
    pub output_buffer_ms: f32,
    pub processing_ms: f32,
    pub reference_wait_ms: f32,
    pub echo_mode: &'static str,
    pub noise_mode: &'static str,
    pub raw_microphone: bool,
    pub audio_priority: bool,
    pub ai_overloaded: bool,
}

#[derive(Debug)]
pub struct HealthLog {
    reported: Counters,
    last_report: Option<Instant>,
    had_problem: bool,
    min_coverage: f32,
    min_buffer_ms: f32,
    max_buffer_ms: f32,
    max_processing_ms: f32,
    max_wait_ms: f32,
}

impl Default for HealthLog {
    fn default() -> Self {
        Self {
            reported: Counters::default(),
            last_report: None,
            had_problem: false,
            min_coverage: 1.0,
            min_buffer_ms: f32::INFINITY,
            max_buffer_ms: 0.0,
            max_processing_ms: 0.0,
            max_wait_ms: 0.0,
        }
    }
}

impl HealthLog {
    /// Report the first problem immediately, then summarize it every 30 seconds. Commands
    /// force a summary so pause, restart, and shutdown do not lose the pending counters.
    pub fn observe(&mut self, now: Health, at: Instant, force: bool) -> Option<HealthReport> {
        if now.counters.decreased(self.reported) {
            *self = Self::default();
        }
        self.min_coverage = self.min_coverage.min(now.coverage);
        self.min_buffer_ms = self.min_buffer_ms.min(now.output_buffer_ms);
        self.max_buffer_ms = self.max_buffer_ms.max(now.output_buffer_ms);
        self.max_processing_ms = self.max_processing_ms.max(now.processing_ms);
        self.max_wait_ms = self.max_wait_ms.max(now.reference_wait_ms);
        let delta = now.counters.since(self.reported);
        let due = self.last_report.is_none_or(|last| at.duration_since(last) >= REPORT_INTERVAL);
        if !force && !due && (self.had_problem || !delta.problems()) {
            return None;
        }
        let report = HealthReport {
            now,
            delta,
            min_coverage: self.min_coverage,
            min_buffer_ms: self.min_buffer_ms,
            max_buffer_ms: self.max_buffer_ms,
            max_processing_ms: self.max_processing_ms,
            max_wait_ms: self.max_wait_ms,
        };
        self.reported = now.counters;
        self.last_report = Some(at);
        self.had_problem = delta.problems();
        self.min_coverage = 1.0;
        self.min_buffer_ms = f32::INFINITY;
        self.max_buffer_ms = 0.0;
        self.max_processing_ms = 0.0;
        self.max_wait_ms = 0.0;
        Some(report)
    }
}

#[derive(Debug)]
pub struct HealthReport {
    now: Health,
    delta: Counters,
    min_coverage: f32,
    min_buffer_ms: f32,
    max_buffer_ms: f32,
    max_processing_ms: f32,
    max_wait_ms: f32,
}

impl HealthReport {
    pub fn has_problems(&self) -> bool {
        self.delta.problems()
    }
}

impl fmt::Display for HealthReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let missing = 100.0 * self.delta.incomplete_reference as f64 / self.delta.frames.max(1) as f64;
        write!(
            f,
            "audio health: processing={} echo={} noise={} raw={} priority={} ai_overloaded={} frames=+{}/{} incomplete_reference=+{}/{} ({missing:.1}%) gaps=+{}/{} trims=+{}/{} dropped=+{}/{} retrained=+{}/{} stream_resets=+{}/{} reference_discontinuities=+{}/{} reference_shift={:.1}ms sampled_coverage_min={:.1}% sampled_buffer={:.1}..{:.1}ms sampled_processing_max={:.2}ms sampled_reference_wait_max={:.2}ms",
            self.now.processing,
            self.now.echo_mode,
            self.now.noise_mode,
            self.now.raw_microphone,
            self.now.audio_priority,
            self.now.ai_overloaded,
            self.delta.frames,
            self.now.counters.frames,
            self.delta.incomplete_reference,
            self.now.counters.incomplete_reference,
            self.delta.gaps,
            self.now.counters.gaps,
            self.delta.trims,
            self.now.counters.trims,
            self.delta.dropped,
            self.now.counters.dropped,
            self.delta.retrained,
            self.now.counters.retrained,
            self.delta.stream_resets,
            self.now.counters.stream_resets,
            self.delta.reference_discontinuities,
            self.now.counters.reference_discontinuities,
            self.now.reference_shift_ms,
            100.0 * self.min_coverage,
            self.min_buffer_ms,
            self.max_buffer_ms,
            self.max_processing_ms,
            self.max_wait_ms
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(frames: u64, incomplete_reference: u64) -> Health {
        Health {
            counters: Counters { frames, incomplete_reference, ..Default::default() },
            processing: true,
            coverage: 1.0,
            output_buffer_ms: 20.0,
            ..Default::default()
        }
    }

    #[test]
    fn persistent_failures_are_aggregated_instead_of_logged_at_every_refresh() {
        let at = Instant::now();
        let mut logger = HealthLog::default();
        let mut reports = Vec::new();
        for tick in 0..=1201 {
            if let Some(report) =
                logger.observe(health(tick * 5, tick * 5), at + Duration::from_millis(tick * 50), false)
            {
                reports.push(report);
            }
        }
        assert_eq!(reports.len(), 4); // startup, first failure, 30-second intervals
        assert_eq!(reports.iter().map(|r| r.delta.incomplete_reference).sum::<u64>(), 6005);
        assert_eq!(reports[2].delta.incomplete_reference, 3000);
        assert!(reports[2].to_string().contains("100.0%"));
    }

    #[test]
    fn a_thirteen_hour_fault_retains_its_counts_with_bounded_log_volume() {
        let at = Instant::now();
        let mut logger = HealthLog::default();
        let (mut reports, mut bytes, mut missing, mut resets) = (0, 0, 0, 0);
        let ticks = 13 * 60 * 60 * 20;
        for tick in 0..=ticks {
            let mut sample = health(tick * 5, tick * 5);
            sample.echo_mode = "clean";
            sample.noise_mode = "ai";
            sample.counters.stream_resets = tick / 10_000;
            if let Some(report) = logger.observe(sample, at + Duration::from_millis(tick * 50), tick == ticks) {
                reports += 1;
                bytes += report.to_string().len() + 32; // timestamp, level and newline
                missing += report.delta.incomplete_reference;
                resets += report.delta.stream_resets;
            }
        }
        assert!(reports <= 1562, "reports: {reports}");
        assert_eq!(missing, ticks * 5);
        assert_eq!(resets, ticks / 10_000);
        assert!(bytes < 3 * (1 << 20), "13 hours of health summaries used {bytes} bytes");
    }

    #[test]
    fn forced_summary_retains_pending_failures_and_sampled_peaks() {
        let at = Instant::now();
        let mut logger = HealthLog::default();
        logger.observe(health(100, 10), at, false).unwrap();
        let mut sample = health(105, 11);
        sample.coverage = 0.2;
        sample.processing_ms = 8.0;
        sample.reference_wait_ms = 12.0;
        sample.output_buffer_ms = 3.0;
        assert!(logger.observe(sample, at + Duration::from_secs(1), false).is_none());
        let mut current = health(110, 12);
        current.counters.gaps = 2;
        current.counters.trims = 3;
        current.counters.dropped = 4;
        let report = logger.observe(current, at + Duration::from_secs(2), true).unwrap();
        assert_eq!(report.delta, Counters { frames: 10, incomplete_reference: 2, gaps: 2, trims: 3, dropped: 4, ..Default::default() });
        assert_eq!(report.min_coverage, 0.2);
        assert_eq!(report.max_processing_ms, 8.0);
        assert_eq!(report.max_wait_ms, 12.0);
        assert_eq!(report.min_buffer_ms, 3.0);
    }

    #[test]
    fn an_engine_counter_reset_does_not_inherit_old_totals_or_peaks() {
        let at = Instant::now();
        let mut logger = HealthLog::default();
        logger.observe(health(1000, 500), at, false).unwrap();
        let mut pending = health(1005, 505);
        pending.processing_ms = 100.0;
        logger.observe(pending, at + Duration::from_secs(1), false);
        let report = logger.observe(health(5, 1), at + Duration::from_secs(2), false).unwrap();
        assert_eq!(report.delta.frames, 5);
        assert_eq!(report.delta.incomplete_reference, 1);
        assert_eq!(report.max_processing_ms, 0.0);
    }

    #[test]
    fn recovery_is_visible_and_a_new_failure_is_reported_promptly() {
        let at = Instant::now();
        let mut logger = HealthLog::default();
        assert!(logger.observe(health(100, 10), at, false).unwrap().has_problems());
        assert!(!logger.observe(health(200, 10), at + REPORT_INTERVAL, false).unwrap().has_problems());
        assert!(
            logger
                .observe(health(205, 11), at + REPORT_INTERVAL + Duration::from_millis(50), false)
                .unwrap()
                .has_problems()
        );
    }
}
