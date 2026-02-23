// Statistics collector module

use dashmap::DashMap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Thread-safe statistics collector using atomic operations.
pub struct StatsCollector {
    total_calls: AtomicU64,
    successful_calls: AtomicU64,
    failed_calls: AtomicU64,
    active_dialogs: AtomicU64,
    auth_failures: AtomicU64,
    status_codes: DashMap<u16, AtomicU64>,
    latencies: Mutex<Vec<Duration>>,
    start_time: Instant,
}

/// A point-in-time snapshot of collected statistics.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub timestamp: Instant,
    pub total_calls: u64,
    pub successful_calls: u64,
    pub failed_calls: u64,
    pub active_dialogs: u64,
    pub auth_failures: u64,
    pub cps: f64,
    pub latency_p50: Duration,
    pub latency_p90: Duration,
    pub latency_p95: Duration,
    pub latency_p99: Duration,
    pub status_codes: HashMap<u16, u64>,
}


impl StatsCollector {
    /// Create a new StatsCollector.
    pub fn new() -> Self {
        Self {
            total_calls: AtomicU64::new(0),
            successful_calls: AtomicU64::new(0),
            failed_calls: AtomicU64::new(0),
            active_dialogs: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            status_codes: DashMap::new(),
            latencies: Mutex::new(Vec::new()),
            start_time: Instant::now(),
        }
    }

    /// Record a successful call with its status code and latency.
    pub fn record_call(&self, status_code: u16, latency: Duration) {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        self.successful_calls.fetch_add(1, Ordering::Relaxed);
        self.status_codes
            .entry(status_code)
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        self.latencies.lock().unwrap().push(latency);
    }

    /// Record a failed call.
    pub fn record_failure(&self) {
        self.total_calls.fetch_add(1, Ordering::Relaxed);
        self.failed_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an authentication failure.
    pub fn record_auth_failure(&self) {
        self.auth_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment the active dialog count.
    pub fn increment_active_dialogs(&self) {
        self.active_dialogs.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the active dialog count.
    pub fn decrement_active_dialogs(&self) {
        self.active_dialogs.fetch_sub(1, Ordering::Relaxed);
    }

    /// Take a snapshot of the current statistics.
    pub fn snapshot(&self) -> StatsSnapshot {
        let now = Instant::now();
        let total = self.total_calls.load(Ordering::Relaxed);
        let elapsed = now.duration_since(self.start_time).as_secs_f64();
        let cps = if elapsed > 0.0 {
            total as f64 / elapsed
        } else {
            0.0
        };

        let latencies = self.latencies.lock().unwrap();
        let (p50, p90, p95, p99) = calculate_percentiles(&latencies);

        let mut status_map = HashMap::new();
        for entry in self.status_codes.iter() {
            status_map.insert(*entry.key(), entry.value().load(Ordering::Relaxed));
        }

        StatsSnapshot {
            timestamp: now,
            total_calls: total,
            successful_calls: self.successful_calls.load(Ordering::Relaxed),
            failed_calls: self.failed_calls.load(Ordering::Relaxed),
            active_dialogs: self.active_dialogs.load(Ordering::Relaxed),
            auth_failures: self.auth_failures.load(Ordering::Relaxed),
            cps,
            latency_p50: p50,
            latency_p90: p90,
            latency_p95: p95,
            latency_p99: p99,
            status_codes: status_map,
        }
    }

    /// Display a formatted stats snapshot to stdout.
    pub fn display_snapshot(snapshot: &StatsSnapshot) {
        println!("--- Stats Snapshot ---");
        println!(
            "Total: {} | Success: {} | Failed: {} | Active: {}",
            snapshot.total_calls,
            snapshot.successful_calls,
            snapshot.failed_calls,
            snapshot.active_dialogs
        );
        println!(
            "CPS: {:.1} | Auth Failures: {}",
            snapshot.cps, snapshot.auth_failures
        );
        println!(
            "Latency p50: {:.1}ms | p90: {:.1}ms | p95: {:.1}ms | p99: {:.1}ms",
            snapshot.latency_p50.as_secs_f64() * 1000.0,
            snapshot.latency_p90.as_secs_f64() * 1000.0,
            snapshot.latency_p95.as_secs_f64() * 1000.0,
            snapshot.latency_p99.as_secs_f64() * 1000.0,
        );
        if !snapshot.status_codes.is_empty() {
            let mut codes: Vec<_> = snapshot.status_codes.iter().collect();
            codes.sort_by_key(|(k, _)| *k);
            let code_strs: Vec<String> = codes.iter().map(|(k, v)| format!("{}:{}", k, v)).collect();
            println!("Status Codes: {}", code_strs.join(" | "));
        }
        println!("---------------------");
    }

    /// Display a final result summary.
    pub fn display_final_summary(snapshot: &StatsSnapshot) {
        println!("=== Final Result Summary ===");
        println!("Total Calls:      {}", snapshot.total_calls);
        println!("Successful Calls: {}", snapshot.successful_calls);
        println!("Failed Calls:     {}", snapshot.failed_calls);
        println!("Auth Failures:    {}", snapshot.auth_failures);
        println!("Average CPS:      {:.1}", snapshot.cps);
        println!(
            "Latency p50: {:.1}ms | p90: {:.1}ms | p95: {:.1}ms | p99: {:.1}ms",
            snapshot.latency_p50.as_secs_f64() * 1000.0,
            snapshot.latency_p90.as_secs_f64() * 1000.0,
            snapshot.latency_p95.as_secs_f64() * 1000.0,
            snapshot.latency_p99.as_secs_f64() * 1000.0,
        );
        if !snapshot.status_codes.is_empty() {
            println!("Status Code Distribution:");
            let mut codes: Vec<_> = snapshot.status_codes.iter().collect();
            codes.sort_by_key(|(k, _)| *k);
            for (code, count) in &codes {
                println!("  {}: {}", code, count);
            }
        }
        println!("============================");
    }
}

/// Calculate percentiles from a slice of durations.
/// Returns (p50, p90, p95, p99). Returns Duration::ZERO for empty input.
pub fn calculate_percentiles(latencies: &[Duration]) -> (Duration, Duration, Duration, Duration) {
    if latencies.is_empty() {
        return (
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
    }

    let mut sorted = latencies.to_vec();
    sorted.sort();

    let len = sorted.len();
    let p50 = percentile_at(&sorted, len, 50.0);
    let p90 = percentile_at(&sorted, len, 90.0);
    let p95 = percentile_at(&sorted, len, 95.0);
    let p99 = percentile_at(&sorted, len, 99.0);

    (p50, p90, p95, p99)
}

/// Get the value at a given percentile from a sorted slice using nearest-rank method.
fn percentile_at(sorted: &[Duration], len: usize, pct: f64) -> Duration {
    if len == 1 {
        return sorted[0];
    }
    // Nearest-rank: index = ceil(pct/100 * len) - 1
    let rank = (pct / 100.0 * len as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(len - 1);
    sorted[idx]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ===== Unit Tests =====

    #[test]
    fn test_new_collector_has_zero_values() {
        let collector = StatsCollector::new();
        let snap = collector.snapshot();
        assert_eq!(snap.total_calls, 0);
        assert_eq!(snap.successful_calls, 0);
        assert_eq!(snap.failed_calls, 0);
        assert_eq!(snap.active_dialogs, 0);
        assert_eq!(snap.auth_failures, 0);
        assert!(snap.status_codes.is_empty());
        assert_eq!(snap.latency_p50, Duration::ZERO);
        assert_eq!(snap.latency_p90, Duration::ZERO);
        assert_eq!(snap.latency_p95, Duration::ZERO);
        assert_eq!(snap.latency_p99, Duration::ZERO);
    }

    #[test]
    fn test_record_call_increments_counters() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        collector.record_call(200, Duration::from_millis(20));
        collector.record_call(404, Duration::from_millis(5));

        let snap = collector.snapshot();
        assert_eq!(snap.total_calls, 3);
        assert_eq!(snap.successful_calls, 3);
        assert_eq!(snap.failed_calls, 0);
        assert_eq!(*snap.status_codes.get(&200).unwrap(), 2);
        assert_eq!(*snap.status_codes.get(&404).unwrap(), 1);
    }

    #[test]
    fn test_record_failure_increments_counters() {
        let collector = StatsCollector::new();
        collector.record_failure();
        collector.record_failure();

        let snap = collector.snapshot();
        assert_eq!(snap.total_calls, 2);
        assert_eq!(snap.failed_calls, 2);
        assert_eq!(snap.successful_calls, 0);
    }

    #[test]
    fn test_record_auth_failure() {
        let collector = StatsCollector::new();
        collector.record_auth_failure();
        collector.record_auth_failure();
        collector.record_auth_failure();

        let snap = collector.snapshot();
        assert_eq!(snap.auth_failures, 3);
    }

    #[test]
    fn test_active_dialogs_increment_decrement() {
        let collector = StatsCollector::new();
        collector.increment_active_dialogs();
        collector.increment_active_dialogs();
        collector.increment_active_dialogs();
        assert_eq!(collector.snapshot().active_dialogs, 3);

        collector.decrement_active_dialogs();
        assert_eq!(collector.snapshot().active_dialogs, 2);
    }

    #[test]
    fn test_percentile_empty_latencies() {
        let (p50, p90, p95, p99) = calculate_percentiles(&[]);
        assert_eq!(p50, Duration::ZERO);
        assert_eq!(p90, Duration::ZERO);
        assert_eq!(p95, Duration::ZERO);
        assert_eq!(p99, Duration::ZERO);
    }

    #[test]
    fn test_percentile_single_element() {
        let latencies = vec![Duration::from_millis(42)];
        let (p50, p90, p95, p99) = calculate_percentiles(&latencies);
        assert_eq!(p50, Duration::from_millis(42));
        assert_eq!(p90, Duration::from_millis(42));
        assert_eq!(p95, Duration::from_millis(42));
        assert_eq!(p99, Duration::from_millis(42));
    }

    #[test]
    fn test_percentile_known_distribution() {
        // 100 values: 1ms, 2ms, ..., 100ms
        let latencies: Vec<Duration> = (1..=100).map(|i| Duration::from_millis(i)).collect();
        let (p50, p90, p95, p99) = calculate_percentiles(&latencies);

        // p50 should be around 50ms, p90 around 90ms, etc.
        assert_eq!(p50, Duration::from_millis(50));
        assert_eq!(p90, Duration::from_millis(90));
        assert_eq!(p95, Duration::from_millis(95));
        assert_eq!(p99, Duration::from_millis(99));
    }

    #[test]
    fn test_percentile_unsorted_input() {
        // Verify that calculate_percentiles sorts internally
        let latencies = vec![
            Duration::from_millis(100),
            Duration::from_millis(1),
            Duration::from_millis(50),
            Duration::from_millis(75),
            Duration::from_millis(25),
        ];
        let (p50, p90, p95, p99) = calculate_percentiles(&latencies);

        // Sorted: [1, 25, 50, 75, 100] (len=5)
        // nearest-rank: idx = ceil(pct/100 * 5) - 1
        // p50: ceil(0.5*5)-1 = 3-1 = 2 -> 50ms
        assert_eq!(p50, Duration::from_millis(50));
        // p90: ceil(0.9*5)-1 = 5-1 = 4 -> 100ms
        assert_eq!(p90, Duration::from_millis(100));
        // p95: ceil(0.95*5)-1 = 5-1 = 4 -> 100ms
        assert_eq!(p95, Duration::from_millis(100));
        // p99: ceil(0.99*5)-1 = 5-1 = 4 -> 100ms
        assert_eq!(p99, Duration::from_millis(100));
    }

    #[test]
    fn test_status_code_aggregation_multiple_codes() {
        let collector = StatsCollector::new();
        for _ in 0..10 {
            collector.record_call(200, Duration::from_millis(1));
        }
        for _ in 0..5 {
            collector.record_call(404, Duration::from_millis(1));
        }
        for _ in 0..3 {
            collector.record_call(500, Duration::from_millis(1));
        }
        collector.record_call(503, Duration::from_millis(1));

        let snap = collector.snapshot();
        assert_eq!(snap.status_codes.len(), 4);
        assert_eq!(*snap.status_codes.get(&200).unwrap(), 10);
        assert_eq!(*snap.status_codes.get(&404).unwrap(), 5);
        assert_eq!(*snap.status_codes.get(&500).unwrap(), 3);
        assert_eq!(*snap.status_codes.get(&503).unwrap(), 1);
    }

    #[test]
    fn test_snapshot_cps_is_non_negative() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        let snap = collector.snapshot();
        assert!(snap.cps >= 0.0);
    }

    #[test]
    fn test_mixed_success_and_failure() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        collector.record_failure();
        collector.record_call(200, Duration::from_millis(20));
        collector.record_failure();

        let snap = collector.snapshot();
        assert_eq!(snap.total_calls, 4);
        assert_eq!(snap.successful_calls, 2);
        assert_eq!(snap.failed_calls, 2);
    }

    #[test]
    fn test_display_snapshot_does_not_panic() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        collector.record_call(404, Duration::from_millis(20));
        collector.record_failure();
        collector.record_auth_failure();
        collector.increment_active_dialogs();

        let snap = collector.snapshot();
        // Just verify it doesn't panic
        StatsCollector::display_snapshot(&snap);
    }

    #[test]
    fn test_display_final_summary_does_not_panic() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        let snap = collector.snapshot();
        StatsCollector::display_final_summary(&snap);
    }

    #[test]
    fn test_display_snapshot_empty_does_not_panic() {
        let collector = StatsCollector::new();
        let snap = collector.snapshot();
        StatsCollector::display_snapshot(&snap);
        StatsCollector::display_final_summary(&snap);
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let collector = Arc::new(StatsCollector::new());
        let mut handles = vec![];

        for _ in 0..10 {
            let c = Arc::clone(&collector);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    c.record_call(200, Duration::from_millis(5));
                    c.increment_active_dialogs();
                    c.decrement_active_dialogs();
                }
            }));
        }

        for _ in 0..5 {
            let c = Arc::clone(&collector);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    c.record_failure();
                    c.record_auth_failure();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let snap = collector.snapshot();
        assert_eq!(snap.total_calls, 10 * 100 + 5 * 100); // 1000 success + 500 failure
        assert_eq!(snap.successful_calls, 1000);
        assert_eq!(snap.failed_calls, 500);
        assert_eq!(snap.auth_failures, 500);
        assert_eq!(snap.active_dialogs, 0); // all incremented then decremented
    }

    #[test]
    fn test_latency_recorded_in_snapshot() {
        let collector = StatsCollector::new();
        collector.record_call(200, Duration::from_millis(10));
        collector.record_call(200, Duration::from_millis(20));
        collector.record_call(200, Duration::from_millis(30));

        let snap = collector.snapshot();
        // With 3 elements [10, 20, 30]:
        // nearest-rank: idx = ceil(0.5 * 3) - 1 = 2 - 1 = 1 -> 20ms
        assert_eq!(snap.latency_p50, Duration::from_millis(20));
    }

    // ===== Property-Based Tests =====

    use proptest::prelude::*;
    use proptest::collection::vec;

    // Feature: sip-load-tester, Property 9: レイテンシパーセンタイル計算
    // **Validates: Requirements 11.2**
    proptest! {
        #[test]
        fn prop_latency_percentile_matches_nearest_rank(
            latencies_ms in vec(1u64..10_000, 1..200)
        ) {
            let latencies: Vec<Duration> = latencies_ms.iter()
                .map(|&ms| Duration::from_millis(ms))
                .collect();

            let (p50, p90, p95, p99) = calculate_percentiles(&latencies);

            let mut sorted: Vec<Duration> = latencies.clone();
            sorted.sort();
            let len = sorted.len();

            // Nearest-rank method: index = ceil(pct/100 * len) - 1
            let expected_p50 = sorted[(50.0_f64 / 100.0 * len as f64).ceil() as usize - 1];
            let expected_p90 = sorted[(90.0_f64 / 100.0 * len as f64).ceil() as usize - 1];
            let expected_p95 = sorted[(95.0_f64 / 100.0 * len as f64).ceil() as usize - 1];
            let expected_p99 = sorted[((99.0_f64 / 100.0 * len as f64).ceil() as usize - 1).min(len - 1)];

            prop_assert_eq!(p50, expected_p50, "p50 mismatch for len={}", len);
            prop_assert_eq!(p90, expected_p90, "p90 mismatch for len={}", len);
            prop_assert_eq!(p95, expected_p95, "p95 mismatch for len={}", len);
            prop_assert_eq!(p99, expected_p99, "p99 mismatch for len={}", len);
        }
    }

    // Feature: sip-load-tester, Property 10: ステータスコード別集計
    // **Validates: Requirements 11.3**
    proptest! {
        #[test]
        fn prop_status_code_aggregation(
            codes in vec(100u16..700, 1..200)
        ) {
            let collector = StatsCollector::new();

            // Record each status code with a dummy latency
            for &code in &codes {
                collector.record_call(code, Duration::from_millis(1));
            }

            let snap = collector.snapshot();

            // Build expected counts from the input sequence
            let mut expected: HashMap<u16, u64> = HashMap::new();
            for &code in &codes {
                *expected.entry(code).or_insert(0) += 1;
            }

            // Verify snapshot status_codes matches expected counts
            prop_assert_eq!(snap.status_codes.len(), expected.len(),
                "number of distinct status codes mismatch");

            for (code, count) in &expected {
                let actual = snap.status_codes.get(code).copied().unwrap_or(0);
                prop_assert_eq!(actual, *count,
                    "count mismatch for status code {}", code);
            }
        }
    }
}
