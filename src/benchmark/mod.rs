use serde::{Deserialize, Serialize};

/// マイクロベンチマーク結果
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkResult {
    pub name: String,
    pub iterations: u64,
    pub mean_ns: f64,
    pub median_ns: f64,
    pub stddev_ns: f64,
    pub throughput_ops_per_sec: f64,
}

/// ベンチマーク比較結果
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct BenchmarkComparison {
    pub name: String,
    pub before: BenchmarkResult,
    pub after: BenchmarkResult,
    pub speedup: f64,
}

impl BenchmarkComparison {
    /// 2つのBenchmarkResultから比較結果を生成する
    /// speedup = before.mean_ns / after.mean_ns（高いほど高速化）
    pub fn from_results(before: BenchmarkResult, after: BenchmarkResult) -> Self {
        let speedup = before.mean_ns / after.mean_ns;
        let name = before.name.clone();
        Self {
            name,
            before,
            after,
            speedup,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn benchmark_result_to_json() {
        let result = BenchmarkResult {
            name: "parse_invite".to_string(),
            iterations: 10000,
            mean_ns: 1500.5,
            median_ns: 1450.0,
            stddev_ns: 120.3,
            throughput_ops_per_sec: 666666.67,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"name\":\"parse_invite\""));
        assert!(json.contains("\"iterations\":10000"));
    }

    #[test]
    fn benchmark_result_from_json() {
        let json = r#"{
            "name": "format_register",
            "iterations": 5000,
            "mean_ns": 2000.0,
            "median_ns": 1900.0,
            "stddev_ns": 200.0,
            "throughput_ops_per_sec": 500000.0
        }"#;
        let result: BenchmarkResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.name, "format_register");
        assert_eq!(result.iterations, 5000);
        assert_eq!(result.mean_ns, 2000.0);
    }

    #[test]
    fn benchmark_result_json_roundtrip() {
        let result = BenchmarkResult {
            name: "headers_get".to_string(),
            iterations: 100000,
            mean_ns: 50.0,
            median_ns: 48.0,
            stddev_ns: 5.5,
            throughput_ops_per_sec: 20000000.0,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: BenchmarkResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result, deserialized);
    }

    #[test]
    fn benchmark_result_pretty_json() {
        let result = BenchmarkResult {
            name: "test".to_string(),
            iterations: 1,
            mean_ns: 1.0,
            median_ns: 1.0,
            stddev_ns: 0.0,
            throughput_ops_per_sec: 1000000000.0,
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        assert!(json.contains('\n'));
    }

    #[test]
    fn benchmark_comparison_to_json() {
        let before = BenchmarkResult {
            name: "parse_invite".to_string(),
            iterations: 10000,
            mean_ns: 3000.0,
            median_ns: 2900.0,
            stddev_ns: 300.0,
            throughput_ops_per_sec: 333333.33,
        };
        let after = BenchmarkResult {
            name: "parse_invite".to_string(),
            iterations: 10000,
            mean_ns: 1500.0,
            median_ns: 1450.0,
            stddev_ns: 120.0,
            throughput_ops_per_sec: 666666.67,
        };
        let comparison = BenchmarkComparison {
            name: "parse_invite".to_string(),
            before,
            after,
            speedup: 2.0,
        };
        let json = serde_json::to_string(&comparison).unwrap();
        assert!(json.contains("\"speedup\":2.0"));
        assert!(json.contains("\"before\""));
        assert!(json.contains("\"after\""));
    }

    #[test]
    fn benchmark_comparison_json_roundtrip() {
        let comparison = BenchmarkComparison {
            name: "format_200_ok".to_string(),
            before: BenchmarkResult {
                name: "format_200_ok".to_string(),
                iterations: 5000,
                mean_ns: 4000.0,
                median_ns: 3800.0,
                stddev_ns: 400.0,
                throughput_ops_per_sec: 250000.0,
            },
            after: BenchmarkResult {
                name: "format_200_ok".to_string(),
                iterations: 5000,
                mean_ns: 2000.0,
                median_ns: 1900.0,
                stddev_ns: 200.0,
                throughput_ops_per_sec: 500000.0,
            },
            speedup: 2.0,
        };
        let json = serde_json::to_string(&comparison).unwrap();
        let deserialized: BenchmarkComparison = serde_json::from_str(&json).unwrap();
        assert_eq!(comparison, deserialized);
    }

    #[test]
    fn benchmark_comparison_from_results() {
        let before = BenchmarkResult {
            name: "test_bench".to_string(),
            iterations: 1000,
            mean_ns: 4000.0,
            median_ns: 3800.0,
            stddev_ns: 400.0,
            throughput_ops_per_sec: 250000.0,
        };
        let after = BenchmarkResult {
            name: "test_bench".to_string(),
            iterations: 1000,
            mean_ns: 2000.0,
            median_ns: 1900.0,
            stddev_ns: 200.0,
            throughput_ops_per_sec: 500000.0,
        };
        let comparison = BenchmarkComparison::from_results(before, after);
        assert_eq!(comparison.name, "test_bench");
        assert_eq!(comparison.speedup, 2.0);
    }

    #[test]
    fn benchmark_comparison_speedup_slower() {
        let before = BenchmarkResult {
            name: "regressed".to_string(),
            iterations: 1000,
            mean_ns: 1000.0,
            median_ns: 950.0,
            stddev_ns: 100.0,
            throughput_ops_per_sec: 1000000.0,
        };
        let after = BenchmarkResult {
            name: "regressed".to_string(),
            iterations: 1000,
            mean_ns: 2000.0,
            median_ns: 1900.0,
            stddev_ns: 200.0,
            throughput_ops_per_sec: 500000.0,
        };
        let comparison = BenchmarkComparison::from_results(before, after);
        assert_eq!(comparison.speedup, 0.5);
    }

    #[test]
    fn benchmark_result_vec_to_json() {
        let results = vec![
            BenchmarkResult {
                name: "bench_a".to_string(),
                iterations: 100,
                mean_ns: 10.0,
                median_ns: 9.0,
                stddev_ns: 1.0,
                throughput_ops_per_sec: 100000000.0,
            },
            BenchmarkResult {
                name: "bench_b".to_string(),
                iterations: 200,
                mean_ns: 20.0,
                median_ns: 19.0,
                stddev_ns: 2.0,
                throughput_ops_per_sec: 50000000.0,
            },
        ];
        let json = serde_json::to_string(&results).unwrap();
        let deserialized: Vec<BenchmarkResult> = serde_json::from_str(&json).unwrap();
        assert_eq!(results, deserialized);
    }

    // Feature: performance-bottleneck-optimization, Property 11: BenchmarkResult の serialize→deserialize ラウンドトリップ
    // **Validates: Requirements 10.3**
    fn arb_benchmark_result() -> impl Strategy<Value = BenchmarkResult> {
        (
            prop::string::string_regex("[a-zA-Z_][a-zA-Z0-9_]{0,30}").unwrap(),
            any::<u64>(),
            (0.001f64..1e15_f64),  // positive finite f64 for mean_ns
            (0.001f64..1e15_f64),  // positive finite f64 for median_ns
            (0.0f64..1e15_f64),    // non-negative finite f64 for stddev_ns
            (0.001f64..1e15_f64),  // positive finite f64 for throughput_ops_per_sec
        )
            .prop_map(|(name, iterations, mean_ns, median_ns, stddev_ns, throughput_ops_per_sec)| {
                BenchmarkResult {
                    name,
                    iterations,
                    mean_ns,
                    median_ns,
                    stddev_ns,
                    throughput_ops_per_sec,
                }
            })
    }

    proptest! {
        #[test]
        fn prop_benchmark_result_json_roundtrip(result in arb_benchmark_result()) {
            let json = serde_json::to_string(&result).unwrap();
            let deserialized: BenchmarkResult = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(result, deserialized);
        }
    }
}
