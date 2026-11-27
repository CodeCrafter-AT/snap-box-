use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::Serialize;

const MAX_RETAINED_SAMPLES: usize = 10_000;

/// Tracked orchestration and filesystem operation categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Allocation,
    WorkspaceClone,
    Release,
    Reset,
    Replenish,
    ColdProvision,
    CommandExec,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencyStats {
    pub count: u64,
    pub failures: u64,
    pub failure_rate: f64,
    pub min_ms: f64,
    pub avg_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub throughput_ops_sec: f64,
}

#[derive(Debug, Default)]
struct OperationBucket {
    samples_ms: VecDeque<f64>,
    total_count: u64,
    failure_count: u64,
    total_duration_ms: f64,
}

impl OperationBucket {
    fn record(&mut self, duration_ms: f64, success: bool) {
        self.total_count += 1;
        if !success {
            self.failure_count += 1;
        }
        self.total_duration_ms += duration_ms;
        if self.samples_ms.len() >= MAX_RETAINED_SAMPLES {
            self.samples_ms.pop_front();
        }
        self.samples_ms.push_back(duration_ms);
    }

    fn summarize(&self, elapsed_secs: f64) -> LatencyStats {
        if self.total_count == 0 {
            return LatencyStats {
                count: 0,
                failures: 0,
                failure_rate: 0.0,
                min_ms: 0.0,
                avg_ms: 0.0,
                median_ms: 0.0,
                p95_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                throughput_ops_sec: 0.0,
            };
        }

        let mut sorted: Vec<f64> = self.samples_ms.iter().copied().collect();
        sorted.sort_by(|a, b| a.total_cmp(b));

        let min_ms = sorted.first().copied().unwrap_or(0.0);
        let max_ms = sorted.last().copied().unwrap_or(0.0);
        let avg_ms = self.total_duration_ms / self.total_count as f64;
        let median_ms = percentile(&sorted, 0.50);
        let p95_ms = percentile(&sorted, 0.95);
        let p99_ms = percentile(&sorted, 0.99);
        let failure_rate = self.failure_count as f64 / self.total_count as f64;
        let throughput_ops_sec = if elapsed_secs > 0.0 {
            self.total_count as f64 / elapsed_secs
        } else {
            0.0
        };

        LatencyStats {
            count: self.total_count,
            failures: self.failure_count,
            failure_rate,
            min_ms,
            avg_ms,
            median_ms,
            p95_ms,
            p99_ms,
            max_ms,
            throughput_ops_sec,
        }
    }
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Computes `LatencyStats` directly from an explicit slice of durations (useful for benchmarks).
pub fn summarize_durations(durations: &[Duration], failures: u64, wall: Duration) -> LatencyStats {
    let mut bucket = OperationBucket::default();
    for d in durations {
        bucket.record(d.as_secs_f64() * 1000.0, true);
    }
    bucket.failure_count = failures;
    bucket.total_count += failures;
    bucket.summarize(wall.as_secs_f64().max(1e-9))
}

/// Thread-safe registry collecting real observed latencies across daemon operations.
#[derive(Debug, Clone)]
pub struct MetricsRegistry {
    started_at: Instant,
    buckets: Arc<Mutex<BTreeMap<OperationKind, OperationBucket>>>,
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            buckets: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn record_success(&self, op: OperationKind, duration: Duration) {
        let mut guard = self.buckets.lock().expect("metrics mutex poisoned");
        guard
            .entry(op)
            .or_default()
            .record(duration.as_secs_f64() * 1000.0, true);
    }

    pub fn record_failure(&self, op: OperationKind, duration: Duration) {
        let mut guard = self.buckets.lock().expect("metrics mutex poisoned");
        guard
            .entry(op)
            .or_default()
            .record(duration.as_secs_f64() * 1000.0, false);
    }

    pub fn snapshot(&self) -> BTreeMap<OperationKind, LatencyStats> {
        let guard = self.buckets.lock().expect("metrics mutex poisoned");
        let elapsed = self.started_at.elapsed().as_secs_f64().max(1e-9);
        guard
            .iter()
            .map(|(k, v)| (*k, v.summarize(elapsed)))
            .collect()
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_percentiles_accurately() {
        let reg = MetricsRegistry::new();
        for ms in 1..=100 {
            reg.record_success(OperationKind::Allocation, Duration::from_millis(ms));
        }
        let snap = reg.snapshot();
        let stats = &snap[&OperationKind::Allocation];
        assert_eq!(stats.count, 100);
        assert_eq!(stats.failures, 0);
        assert!((stats.min_ms - 1.0).abs() < 0.01);
        assert!((stats.max_ms - 100.0).abs() < 0.01);
        assert!((stats.median_ms - 50.0).abs() <= 1.1);
        assert!(stats.p95_ms >= 94.0);
        assert!(stats.p99_ms >= 98.0);
    }
}
