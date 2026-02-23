// Reporter module - Result data models and JSON output
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::config::Config;

/// 各ステップの結果
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    pub step: usize,
    pub phase: String,        // "expand" or "binary_search"
    pub target_cps: f64,
    pub total_calls: u64,
    pub successful_calls: u64,
    pub failed_calls: u64,
    pub error_rate: f64,
    pub result: String,       // "pass" or "fail"
}

/// 実験結果
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub config: Config,
    pub total_calls: u64,
    pub successful_calls: u64,
    pub failed_calls: u64,
    pub auth_failures: u64,
    pub max_stable_cps: f64,
    pub latency_p50_ms: f64,
    pub latency_p90_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub status_codes: HashMap<u16, u64>,
    pub started_at: String,
    pub finished_at: String,
    #[serde(default)]
    pub steps: Vec<StepReport>,
}

/// 結果比較レポート
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub cps_change_pct: f64,
    pub latency_p50_change_pct: f64,
    pub latency_p90_change_pct: f64,
    pub latency_p95_change_pct: f64,
    pub latency_p99_change_pct: f64,
    pub error_rate_change: f64,
    pub improvements: Vec<String>,
    pub regressions: Vec<String>,
}

/// JSON結果をファイルに書き出す
pub fn write_json_result(result: &ExperimentResult, path: &Path) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(result)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// パーセンテージ変化を計算する。previous が 0 の場合は 0.0 を返す。
fn pct_change(current: f64, previous: f64) -> f64 {
    if previous == 0.0 {
        0.0
    } else {
        (current - previous) / previous * 100.0
    }
}

/// 2つの実験結果を比較する
pub fn compare_results(current: &ExperimentResult, previous: &ExperimentResult) -> ComparisonReport {
    let cps_change_pct = pct_change(current.max_stable_cps, previous.max_stable_cps);

    let latency_p50_change_pct = pct_change(current.latency_p50_ms, previous.latency_p50_ms);
    let latency_p90_change_pct = pct_change(current.latency_p90_ms, previous.latency_p90_ms);
    let latency_p95_change_pct = pct_change(current.latency_p95_ms, previous.latency_p95_ms);
    let latency_p99_change_pct = pct_change(current.latency_p99_ms, previous.latency_p99_ms);

    let current_error_rate = if current.total_calls == 0 {
        0.0
    } else {
        current.failed_calls as f64 / current.total_calls as f64
    };
    let previous_error_rate = if previous.total_calls == 0 {
        0.0
    } else {
        previous.failed_calls as f64 / previous.total_calls as f64
    };
    let error_rate_change = current_error_rate - previous_error_rate;

    let mut improvements = Vec::new();
    let mut regressions = Vec::new();

    // CPS: higher is better (positive change = improvement)
    if cps_change_pct > 0.0 {
        improvements.push(format!("CPS improved by {:.1}%", cps_change_pct));
    } else if cps_change_pct < 0.0 {
        regressions.push(format!("CPS regressed by {:.1}%", cps_change_pct.abs()));
    }

    // Latency: lower is better (negative change = improvement)
    for (name, change) in [
        ("p50 latency", latency_p50_change_pct),
        ("p90 latency", latency_p90_change_pct),
        ("p95 latency", latency_p95_change_pct),
        ("p99 latency", latency_p99_change_pct),
    ] {
        if change < 0.0 {
            improvements.push(format!("{} improved by {:.1}%", name, change.abs()));
        } else if change > 0.0 {
            regressions.push(format!("{} regressed by {:.1}%", name, change));
        }
    }

    // Error rate: lower is better (negative change = improvement)
    if error_rate_change < 0.0 {
        improvements.push(format!(
            "Error rate improved by {:.4}",
            error_rate_change.abs()
        ));
    } else if error_rate_change > 0.0 {
        regressions.push(format!("Error rate regressed by {:.4}", error_rate_change));
    }

    ComparisonReport {
        cps_change_pct,
        latency_p50_change_pct,
        latency_p90_change_pct,
        latency_p95_change_pct,
        latency_p99_change_pct,
        error_rate_change,
        improvements,
        regressions,
    }
}

// Feature: sip-load-tester, Property 11: JSON結果のラウンドトリップ
#[cfg(test)]
pub mod generators {
    use super::*;
    use crate::config::generators::arb_config;
    use proptest::prelude::*;
    use proptest::collection::hash_map;

    /// Strategy for generating valid SIP status codes (1xx-6xx)
    fn arb_sip_status_code() -> impl Strategy<Value = u16> {
        prop_oneof![
            Just(100u16),
            Just(180),
            Just(200),
            Just(302),
            Just(400),
            Just(401),
            Just(403),
            Just(404),
            Just(407),
            Just(408),
            Just(480),
            Just(481),
            Just(486),
            Just(487),
            Just(500),
            Just(503),
            Just(600),
            Just(603),
        ]
    }

    /// Strategy for generating ISO 8601-like date strings
    fn arb_iso8601_datetime() -> impl Strategy<Value = String> {
        (2020u32..2030, 1u32..=12, 1u32..=28, 0u32..24, 0u32..60, 0u32..60)
            .prop_map(|(y, m, d, h, min, s)| {
                format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, min, s)
            })
    }

    /// Strategy for generating HashMap<u16, u64> with valid SIP status codes
    fn arb_status_codes() -> impl Strategy<Value = HashMap<u16, u64>> {
        hash_map(arb_sip_status_code(), 0u64..100_000, 0..8)
    }

    /// Strategy for generating valid ExperimentResult structs
    pub fn arb_experiment_result() -> impl Strategy<Value = ExperimentResult> {
        // Group 1: config + call counts (5 fields)
        let counts = (
            arb_config(),
            0u64..1_000_000,    // total_calls
            0u64..1_000_000,    // successful_calls
            0u64..1_000_000,    // failed_calls
            0u64..100_000,      // auth_failures
        );

        // Group 2: latency/cps + timestamps (7 fields)
        let metrics = (
            0.0f64..10_000.0,   // max_stable_cps
            0.0f64..1_000.0,    // latency_p50_ms
            0.0f64..1_000.0,    // latency_p90_ms
            0.0f64..1_000.0,    // latency_p95_ms
            0.0f64..1_000.0,    // latency_p99_ms
            arb_status_codes(),
            arb_iso8601_datetime(),
        );

        (counts, metrics, arb_iso8601_datetime()).prop_map(
            |((config, total, success, failed, auth), (cps, p50, p90, p95, p99, codes, start), end)| {
                ExperimentResult {
                    config,
                    total_calls: total,
                    successful_calls: success,
                    failed_calls: failed,
                    auth_failures: auth,
                    max_stable_cps: cps,
                    latency_p50_ms: p50,
                    latency_p90_ms: p90,
                    latency_p95_ms: p95,
                    latency_p99_ms: p99,
                    status_codes: codes,
                    started_at: start,
                    finished_at: end,
                    steps: vec![],
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    /// テスト用のExperimentResultを生成するヘルパー
    fn make_result(
        total: u64,
        success: u64,
        failed: u64,
        cps: f64,
        p50: f64,
        p90: f64,
        p95: f64,
        p99: f64,
    ) -> ExperimentResult {
        let mut status_codes = HashMap::new();
        if success > 0 {
            status_codes.insert(200, success);
        }
        if failed > 0 {
            status_codes.insert(500, failed);
        }
        ExperimentResult {
            config: Config::default(),
            total_calls: total,
            successful_calls: success,
            failed_calls: failed,
            auth_failures: 0,
            max_stable_cps: cps,
            latency_p50_ms: p50,
            latency_p90_ms: p90,
            latency_p95_ms: p95,
            latency_p99_ms: p99,
            status_codes,
            started_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: "2024-01-01T00:01:00Z".to_string(),
            steps: vec![],
        }
    }

    // ===== StepReport テスト =====

    #[test]
    fn test_step_report_construction() {
        let step = StepReport {
            step: 1,
            phase: "expand".to_string(),
            target_cps: 100.0,
            total_calls: 500,
            successful_calls: 480,
            failed_calls: 20,
            error_rate: 0.04,
            result: "pass".to_string(),
        };
        assert_eq!(step.step, 1);
        assert_eq!(step.phase, "expand");
        assert_eq!(step.target_cps, 100.0);
        assert_eq!(step.total_calls, 500);
        assert_eq!(step.successful_calls, 480);
        assert_eq!(step.failed_calls, 20);
        assert_eq!(step.error_rate, 0.04);
        assert_eq!(step.result, "pass");
    }

    #[test]
    fn test_step_report_serde_roundtrip() {
        let step = StepReport {
            step: 3,
            phase: "binary_search".to_string(),
            target_cps: 250.0,
            total_calls: 1000,
            successful_calls: 900,
            failed_calls: 100,
            error_rate: 0.1,
            result: "fail".to_string(),
        };
        let json = serde_json::to_string(&step).unwrap();
        let deserialized: StepReport = serde_json::from_str(&json).unwrap();
        assert_eq!(step, deserialized);
    }

    #[test]
    fn test_experiment_result_with_steps() {
        let mut result = make_result(1000, 950, 50, 100.0, 5.0, 10.0, 15.0, 20.0);
        result.steps = vec![
            StepReport {
                step: 1,
                phase: "expand".to_string(),
                target_cps: 50.0,
                total_calls: 200,
                successful_calls: 200,
                failed_calls: 0,
                error_rate: 0.0,
                result: "pass".to_string(),
            },
            StepReport {
                step: 2,
                phase: "expand".to_string(),
                target_cps: 100.0,
                total_calls: 400,
                successful_calls: 380,
                failed_calls: 20,
                error_rate: 0.05,
                result: "pass".to_string(),
            },
        ];
        assert_eq!(result.steps.len(), 2);
        assert_eq!(result.steps[0].phase, "expand");
        assert_eq!(result.steps[1].target_cps, 100.0);
    }

    #[test]
    fn test_experiment_result_steps_default_empty() {
        let result = make_result(100, 90, 10, 50.0, 3.0, 6.0, 9.0, 12.0);
        assert!(result.steps.is_empty());
    }

    #[test]
    fn test_experiment_result_serde_backward_compat_without_steps() {
        // JSON without "steps" field should deserialize with steps defaulting to empty vec
        let json = r#"{
            "config": {"mode": "sustained", "target_cps": 10.0, "duration": 60, "proxy_host": "127.0.0.1", "proxy_port": 5060, "uac_host": "127.0.0.1", "uac_port": 5070, "uas_host": "127.0.0.1", "uas_port": 5080, "scenario": "register", "pattern": {"type": "sustained", "duration_secs": 60}, "health_check_retries": 3, "health_check_timeout": 5, "shutdown_timeout": 10},
            "total_calls": 100,
            "successful_calls": 90,
            "failed_calls": 10,
            "auth_failures": 0,
            "max_stable_cps": 10.0,
            "latency_p50_ms": 5.0,
            "latency_p90_ms": 10.0,
            "latency_p95_ms": 15.0,
            "latency_p99_ms": 20.0,
            "status_codes": {},
            "started_at": "2024-01-01T00:00:00Z",
            "finished_at": "2024-01-01T00:01:00Z"
        }"#;
        let result: ExperimentResult = serde_json::from_str(json).unwrap();
        assert!(result.steps.is_empty());
    }

    #[test]
    fn test_experiment_result_serde_roundtrip_with_steps() {
        let mut result = make_result(1000, 950, 50, 100.0, 5.0, 10.0, 15.0, 20.0);
        result.steps = vec![
            StepReport {
                step: 1,
                phase: "expand".to_string(),
                target_cps: 50.0,
                total_calls: 500,
                successful_calls: 490,
                failed_calls: 10,
                error_rate: 0.02,
                result: "pass".to_string(),
            },
        ];
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ExperimentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
        assert_eq!(deserialized.steps.len(), 1);
        assert_eq!(deserialized.steps[0].phase, "expand");
    }

    // ===== ExperimentResult構築テスト =====

    #[test]
    fn test_experiment_result_construction() {
        let result = make_result(1000, 950, 50, 100.0, 5.0, 10.0, 15.0, 20.0);
        assert_eq!(result.total_calls, 1000);
        assert_eq!(result.successful_calls, 950);
        assert_eq!(result.failed_calls, 50);
        assert_eq!(result.max_stable_cps, 100.0);
        assert_eq!(result.latency_p50_ms, 5.0);
        assert_eq!(result.latency_p90_ms, 10.0);
        assert_eq!(result.latency_p95_ms, 15.0);
        assert_eq!(result.latency_p99_ms, 20.0);
        assert_eq!(result.status_codes.get(&200), Some(&950));
        assert_eq!(result.status_codes.get(&500), Some(&50));
    }

    #[test]
    fn test_experiment_result_with_empty_status_codes() {
        let result = ExperimentResult {
            config: Config::default(),
            total_calls: 0,
            successful_calls: 0,
            failed_calls: 0,
            auth_failures: 0,
            max_stable_cps: 0.0,
            latency_p50_ms: 0.0,
            latency_p90_ms: 0.0,
            latency_p95_ms: 0.0,
            latency_p99_ms: 0.0,
            status_codes: HashMap::new(),
            started_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: "2024-01-01T00:00:00Z".to_string(),
            steps: vec![],
        };
        assert!(result.status_codes.is_empty());
    }

    // ===== Serde ラウンドトリップテスト =====

    #[test]
    fn test_experiment_result_serde_roundtrip() {
        let result = make_result(1000, 950, 50, 100.0, 5.0, 10.0, 15.0, 20.0);
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ExperimentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_experiment_result_serde_roundtrip_empty() {
        let result = ExperimentResult {
            config: Config::default(),
            total_calls: 0,
            successful_calls: 0,
            failed_calls: 0,
            auth_failures: 0,
            max_stable_cps: 0.0,
            latency_p50_ms: 0.0,
            latency_p90_ms: 0.0,
            latency_p95_ms: 0.0,
            latency_p99_ms: 0.0,
            status_codes: HashMap::new(),
            started_at: String::new(),
            finished_at: String::new(),
            steps: vec![],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: ExperimentResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn test_comparison_report_serde_roundtrip() {
        let report = ComparisonReport {
            cps_change_pct: 10.5,
            latency_p50_change_pct: -5.0,
            latency_p90_change_pct: -3.0,
            latency_p95_change_pct: 2.0,
            latency_p99_change_pct: 0.0,
            error_rate_change: -0.01,
            improvements: vec!["CPS improved by 10.5%".to_string()],
            regressions: vec!["p95 latency regressed by 2.0%".to_string()],
        };
        let json = serde_json::to_string(&report).unwrap();
        let deserialized: ComparisonReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, deserialized);
    }

    // ===== write_json_result テスト =====

    #[test]
    fn test_write_json_result_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("result.json");
        let result = make_result(100, 90, 10, 50.0, 3.0, 6.0, 9.0, 12.0);

        write_json_result(&result, &path).unwrap();

        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: ExperimentResult = serde_json::from_str(&content).unwrap();
        assert_eq!(result, loaded);
    }

    #[test]
    fn test_write_json_result_pretty_printed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("result.json");
        let result = make_result(100, 90, 10, 50.0, 3.0, 6.0, 9.0, 12.0);

        write_json_result(&result, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // Pretty-printed JSON contains newlines and indentation
        assert!(content.contains('\n'));
        assert!(content.contains("  "));
    }

    #[test]
    fn test_write_json_result_invalid_path() {
        let result = make_result(100, 90, 10, 50.0, 3.0, 6.0, 9.0, 12.0);
        let bad_path = Path::new("/nonexistent_dir_12345/result.json");
        let res = write_json_result(&result, bad_path);
        assert!(res.is_err());
    }

    // ===== compare_results テスト =====

    #[test]
    fn test_compare_results_improvement() {
        // CPS increased, latency decreased, error rate decreased
        let previous = make_result(1000, 900, 100, 100.0, 10.0, 20.0, 30.0, 40.0);
        let current = make_result(1000, 950, 50, 120.0, 8.0, 16.0, 24.0, 32.0);

        let report = compare_results(&current, &previous);

        // CPS: (120 - 100) / 100 * 100 = 20%
        assert!((report.cps_change_pct - 20.0).abs() < 0.01);
        // p50: (8 - 10) / 10 * 100 = -20%
        assert!((report.latency_p50_change_pct - (-20.0)).abs() < 0.01);
        // p90: (16 - 20) / 20 * 100 = -20%
        assert!((report.latency_p90_change_pct - (-20.0)).abs() < 0.01);
        // Error rate: 50/1000 - 100/1000 = -0.05
        assert!((report.error_rate_change - (-0.05)).abs() < 0.001);

        assert!(!report.improvements.is_empty());
        assert!(report.regressions.is_empty());
    }

    #[test]
    fn test_compare_results_regression() {
        // CPS decreased, latency increased, error rate increased
        let previous = make_result(1000, 950, 50, 120.0, 8.0, 16.0, 24.0, 32.0);
        let current = make_result(1000, 900, 100, 100.0, 10.0, 20.0, 30.0, 40.0);

        let report = compare_results(&current, &previous);

        // CPS: (100 - 120) / 120 * 100 ≈ -16.67%
        assert!(report.cps_change_pct < 0.0);
        // Latencies increased
        assert!(report.latency_p50_change_pct > 0.0);
        assert!(report.latency_p90_change_pct > 0.0);
        // Error rate increased
        assert!(report.error_rate_change > 0.0);

        assert!(report.improvements.is_empty());
        assert!(!report.regressions.is_empty());
    }

    #[test]
    fn test_compare_results_no_change() {
        let result = make_result(1000, 950, 50, 100.0, 10.0, 20.0, 30.0, 40.0);

        let report = compare_results(&result, &result);

        assert!((report.cps_change_pct).abs() < 0.001);
        assert!((report.latency_p50_change_pct).abs() < 0.001);
        assert!((report.error_rate_change).abs() < 0.001);
        assert!(report.improvements.is_empty());
        assert!(report.regressions.is_empty());
    }

    #[test]
    fn test_compare_results_division_by_zero_previous_zero_cps() {
        let previous = make_result(1000, 950, 50, 0.0, 0.0, 0.0, 0.0, 0.0);
        let current = make_result(1000, 950, 50, 100.0, 10.0, 20.0, 30.0, 40.0);

        let report = compare_results(&current, &previous);

        // When previous is 0, pct_change returns 0.0
        assert_eq!(report.cps_change_pct, 0.0);
        assert_eq!(report.latency_p50_change_pct, 0.0);
    }

    #[test]
    fn test_compare_results_zero_total_calls() {
        let previous = make_result(0, 0, 0, 100.0, 10.0, 20.0, 30.0, 40.0);
        let current = make_result(0, 0, 0, 100.0, 10.0, 20.0, 30.0, 40.0);

        let report = compare_results(&current, &previous);

        // Error rate should be 0 when total_calls is 0
        assert_eq!(report.error_rate_change, 0.0);
    }

    #[test]
    fn test_compare_results_mixed_improvements_and_regressions() {
        // CPS improved but latency regressed
        let previous = make_result(1000, 950, 50, 100.0, 10.0, 20.0, 30.0, 40.0);
        let current = make_result(1000, 950, 50, 120.0, 12.0, 24.0, 36.0, 48.0);

        let report = compare_results(&current, &previous);

        assert!(report.cps_change_pct > 0.0); // CPS improved
        assert!(report.latency_p50_change_pct > 0.0); // latency regressed

        assert!(report.improvements.iter().any(|s| s.contains("CPS")));
        assert!(report.regressions.iter().any(|s| s.contains("latency")));
    }

    // ===== pct_change ヘルパーテスト =====

    #[test]
    fn test_pct_change_positive() {
        assert!((pct_change(120.0, 100.0) - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_pct_change_negative() {
        assert!((pct_change(80.0, 100.0) - (-20.0)).abs() < 0.001);
    }

    #[test]
    fn test_pct_change_zero_previous() {
        assert_eq!(pct_change(100.0, 0.0), 0.0);
    }

    #[test]
    fn test_pct_change_no_change() {
        assert_eq!(pct_change(100.0, 100.0), 0.0);
    }

    // ===== Property 11: JSON結果のラウンドトリップ =====

    use proptest::prelude::*;

    proptest! {
        // Feature: sip-load-tester, Property 11: JSON結果のラウンドトリップ
        // **Validates: Requirements 12.2, 12.3**
        #[test]
        fn prop_experiment_result_json_roundtrip(
            result in super::generators::arb_experiment_result()
        ) {
            let json = serde_json::to_string(&result).unwrap();
            let deserialized: ExperimentResult = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(&result, &deserialized);
        }

        // Feature: sip-load-tester, Property 12: 結果比較の正確性
        // **Validates: Requirements 13.2, 13.3**
        #[test]
        fn prop_compare_results_accuracy(
            current in super::generators::arb_experiment_result(),
            previous in super::generators::arb_experiment_result(),
        ) {
            let report = compare_results(&current, &previous);
            let tolerance = 0.001;

            // 1. CPS change percentage matches manual calculation
            let expected_cps_change = if previous.max_stable_cps == 0.0 {
                0.0
            } else {
                (current.max_stable_cps - previous.max_stable_cps) / previous.max_stable_cps * 100.0
            };
            prop_assert!(
                (report.cps_change_pct - expected_cps_change).abs() < tolerance,
                "CPS change: expected {}, got {}", expected_cps_change, report.cps_change_pct
            );

            // 2. Latency change percentages match manual calculation for p50, p90, p95, p99
            let expected_p50 = if previous.latency_p50_ms == 0.0 { 0.0 }
                else { (current.latency_p50_ms - previous.latency_p50_ms) / previous.latency_p50_ms * 100.0 };
            let expected_p90 = if previous.latency_p90_ms == 0.0 { 0.0 }
                else { (current.latency_p90_ms - previous.latency_p90_ms) / previous.latency_p90_ms * 100.0 };
            let expected_p95 = if previous.latency_p95_ms == 0.0 { 0.0 }
                else { (current.latency_p95_ms - previous.latency_p95_ms) / previous.latency_p95_ms * 100.0 };
            let expected_p99 = if previous.latency_p99_ms == 0.0 { 0.0 }
                else { (current.latency_p99_ms - previous.latency_p99_ms) / previous.latency_p99_ms * 100.0 };

            prop_assert!(
                (report.latency_p50_change_pct - expected_p50).abs() < tolerance,
                "p50 change: expected {}, got {}", expected_p50, report.latency_p50_change_pct
            );
            prop_assert!(
                (report.latency_p90_change_pct - expected_p90).abs() < tolerance,
                "p90 change: expected {}, got {}", expected_p90, report.latency_p90_change_pct
            );
            prop_assert!(
                (report.latency_p95_change_pct - expected_p95).abs() < tolerance,
                "p95 change: expected {}, got {}", expected_p95, report.latency_p95_change_pct
            );
            prop_assert!(
                (report.latency_p99_change_pct - expected_p99).abs() < tolerance,
                "p99 change: expected {}, got {}", expected_p99, report.latency_p99_change_pct
            );

            // 3. Error rate change matches manual calculation
            let current_error_rate = if current.total_calls == 0 { 0.0 }
                else { current.failed_calls as f64 / current.total_calls as f64 };
            let previous_error_rate = if previous.total_calls == 0 { 0.0 }
                else { previous.failed_calls as f64 / previous.total_calls as f64 };
            let expected_error_rate_change = current_error_rate - previous_error_rate;
            prop_assert!(
                (report.error_rate_change - expected_error_rate_change).abs() < tolerance,
                "Error rate change: expected {}, got {}", expected_error_rate_change, report.error_rate_change
            );

            // 4. When CPS change > 0, improvements contains a CPS entry
            if expected_cps_change > 0.0 {
                prop_assert!(
                    report.improvements.iter().any(|s| s.contains("CPS")),
                    "CPS change {} > 0 but no CPS improvement entry found", expected_cps_change
                );
            }

            // 5. When CPS change < 0, regressions contains a CPS entry
            if expected_cps_change < 0.0 {
                prop_assert!(
                    report.regressions.iter().any(|s| s.contains("CPS")),
                    "CPS change {} < 0 but no CPS regression entry found", expected_cps_change
                );
            }

            // 6. When latency change < 0, improvements contains a latency entry (lower is better)
            let latency_changes = [
                ("p50 latency", expected_p50),
                ("p90 latency", expected_p90),
                ("p95 latency", expected_p95),
                ("p99 latency", expected_p99),
            ];
            for (name, change) in &latency_changes {
                if *change < 0.0 {
                    prop_assert!(
                        report.improvements.iter().any(|s| s.contains(name)),
                        "{} change {} < 0 but no improvement entry found", name, change
                    );
                }
            }

            // 7. When latency change > 0, regressions contains a latency entry
            for (name, change) in &latency_changes {
                if *change > 0.0 {
                    prop_assert!(
                        report.regressions.iter().any(|s| s.contains(name)),
                        "{} change {} > 0 but no regression entry found", name, change
                    );
                }
            }
        }
    }
}
