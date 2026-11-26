//! `/api/state` 各段的耗時（db-perf 2026-11-26）：總時間超過 [`SLOW_MS`] 才寫一行 log，平常不出聲。

use std::time::{Duration, Instant};

pub const SLOW_MS: u128 = 200;

pub struct SegTimer {
    start: Instant,
    last: Instant,
    parts: Vec<(&'static str, Duration)>,
}

impl SegTimer {
    pub fn start() -> Self {
        let now = Instant::now();
        Self { start: now, last: now, parts: Vec::new() }
    }

    pub fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        self.parts.push((name, now - self.last));
        self.last = now;
    }

    pub fn finish(self) {
        self.finish_with(&[])
    }

    pub fn finish_with(self, extra: &[(&'static str, Duration)]) {
        let total = self.start.elapsed();
        if total.as_millis() <= SLOW_MS {
            return;
        }
        let mut parts: Vec<String> = self.parts.iter().map(|(n, d)| format!("{n}={}", d.as_millis())).collect();
        parts.extend(extra.iter().map(|(n, d)| format!("{n}={}", d.as_millis())));
        tracing::info!(total_ms = total.as_millis() as u64, segments = %parts.join(" "), "slow /api/state");
    }
}
