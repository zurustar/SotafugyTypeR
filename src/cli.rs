// CLI subcommand definitions using clap derive macros
use clap::Parser;
use std::path::{Path, PathBuf};

use crate::config::RunMode;
use crate::error::SipLoadTestError;
use crate::reporter::{compare_results, ExperimentResult};
use crate::user_pool::UserGenerator;

/// SIP負荷試験ツール
#[derive(Parser, Debug, PartialEq)]
#[command(name = "sip-load-test")]
pub enum Cli {
    /// ユーザファイルを生成する
    GenerateUsers {
        /// ユーザ名プレフィックス
        #[arg(long, default_value = "user")]
        prefix: String,
        /// 開始番号
        #[arg(long, default_value_t = 1)]
        start: u32,
        /// 生成ユーザ数
        #[arg(long)]
        count: u32,
        /// SIPドメイン
        #[arg(long)]
        domain: String,
        /// パスワードパターン（例: "pass{index}"）
        #[arg(long, default_value = "pass{index}")]
        password_pattern: String,
        /// 出力ファイルパス
        #[arg(short, long)]
        output: PathBuf,
        /// 既存ファイルへの追記
        #[arg(long)]
        append: bool,
    },
    /// 負荷試験を実行する
    Run {
        /// JSON設定ファイルパス
        config: PathBuf,
        /// 実行モード: sustained|step-up|binary-search
        #[arg(long, default_value = "sustained")]
        mode: RunMode,
        /// JSON結果出力先
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// 結果を比較する
    Compare {
        /// 現在の結果JSONファイル
        current: PathBuf,
        /// 過去の結果JSONファイル
        previous: PathBuf,
    },
}


/// generate-usersサブコマンドの実行
///
/// CLI引数からUserGenerator::generateを呼び出し、
/// --appendフラグに応じてwrite_to_fileまたはappend_to_fileを使用する。
pub fn run_generate_users(
    prefix: &str,
    start: u32,
    count: u32,
    domain: &str,
    password_pattern: &str,
    output: &Path,
    append: bool,
) -> Result<(), SipLoadTestError> {
    let users_file = UserGenerator::generate(prefix, start, count, domain, password_pattern);
    let result = if append {
        UserGenerator::append_to_file(&users_file, output)
    } else {
        UserGenerator::write_to_file(&users_file, output)
    };
    result.map_err(|e| SipLoadTestError::UserPoolError(e.to_string()))
}


/// compareサブコマンドの実行
///
/// 2つのJSON結果ファイルを読み込み、compare_resultsで比較し、
/// 比較レポートをJSON形式で標準出力に表示する。
pub fn run_compare(current_path: &Path, previous_path: &Path) -> Result<(), SipLoadTestError> {
    let current_json = std::fs::read_to_string(current_path).map_err(|e| {
        SipLoadTestError::ConfigError(format!(
            "Failed to read current result file '{}': {}",
            current_path.display(),
            e
        ))
    })?;
    let previous_json = std::fs::read_to_string(previous_path).map_err(|e| {
        SipLoadTestError::ConfigError(format!(
            "Failed to read previous result file '{}': {}",
            previous_path.display(),
            e
        ))
    })?;

    let current: ExperimentResult = serde_json::from_str(&current_json).map_err(|e| {
        SipLoadTestError::ConfigError(format!("Failed to parse current result JSON: {}", e))
    })?;
    let previous: ExperimentResult = serde_json::from_str(&previous_json).map_err(|e| {
        SipLoadTestError::ConfigError(format!("Failed to parse previous result JSON: {}", e))
    })?;

    let report = compare_results(&current, &previous);
    let report_json = serde_json::to_string_pretty(&report).map_err(|e| {
        SipLoadTestError::ConfigError(format!("Failed to serialize comparison report: {}", e))
    })?;
    println!("{}", report_json);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    // === generate-users サブコマンドテスト ===

    #[test]
    fn test_generate_users_with_all_required_args() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--count", "100",
            "--domain", "example.com",
            "-o", "users.json",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::GenerateUsers { count, domain, output, prefix, start, password_pattern, append } => {
                assert_eq!(count, 100);
                assert_eq!(domain, "example.com");
                assert_eq!(output, PathBuf::from("users.json"));
                // defaults
                assert_eq!(prefix, "user");
                assert_eq!(start, 1);
                assert_eq!(password_pattern, "pass{index}");
                assert!(!append);
            }
            _ => panic!("Expected GenerateUsers"),
        }
    }

    #[test]
    fn test_generate_users_with_all_args() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--prefix", "sip",
            "--start", "100",
            "--count", "50",
            "--domain", "test.local",
            "--password-pattern", "secret{index}",
            "-o", "/tmp/out.json",
            "--append",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::GenerateUsers { prefix, start, count, domain, password_pattern, output, append } => {
                assert_eq!(prefix, "sip");
                assert_eq!(start, 100);
                assert_eq!(count, 50);
                assert_eq!(domain, "test.local");
                assert_eq!(password_pattern, "secret{index}");
                assert_eq!(output, PathBuf::from("/tmp/out.json"));
                assert!(append);
            }
            _ => panic!("Expected GenerateUsers"),
        }
    }

    #[test]
    fn test_generate_users_missing_required_count() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--domain", "example.com",
            "-o", "users.json",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn test_generate_users_missing_required_domain() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--count", "10",
            "-o", "users.json",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn test_generate_users_missing_required_output() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--count", "10",
            "--domain", "example.com",
        ]);
        assert!(cli.is_err());
    }

    // === run サブコマンドテスト ===

    #[test]
    fn test_run_with_config_path_only() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
            "config.json",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::Run { config, mode, output } => {
                assert_eq!(config, PathBuf::from("config.json"));
                assert_eq!(mode, RunMode::Sustained); // default
                assert!(output.is_none());
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_run_with_mode_and_output() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
            "config.json",
            "--mode", "step-up",
            "--output", "result.json",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::Run { config, mode, output } => {
                assert_eq!(config, PathBuf::from("config.json"));
                assert_eq!(mode, RunMode::StepUp);
                assert_eq!(output, Some(PathBuf::from("result.json")));
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_run_with_binary_search_mode() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
            "config.json",
            "--mode", "binary-search",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::Run { mode, .. } => {
                assert_eq!(mode, RunMode::BinarySearch);
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_run_with_sustained_mode_explicit() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
            "config.json",
            "--mode", "sustained",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::Run { mode, .. } => {
                assert_eq!(mode, RunMode::Sustained);
            }
            _ => panic!("Expected Run"),
        }
    }

    #[test]
    fn test_run_missing_config_path() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn test_run_invalid_mode() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "run",
            "config.json",
            "--mode", "invalid-mode",
        ]);
        assert!(cli.is_err());
    }

    // === compare サブコマンドテスト ===

    #[test]
    fn test_compare_with_two_paths() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "compare",
            "current.json",
            "previous.json",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::Compare { current, previous } => {
                assert_eq!(current, PathBuf::from("current.json"));
                assert_eq!(previous, PathBuf::from("previous.json"));
            }
            _ => panic!("Expected Compare"),
        }
    }

    #[test]
    fn test_compare_missing_previous() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "compare",
            "current.json",
        ]);
        assert!(cli.is_err());
    }

    #[test]
    fn test_compare_missing_both() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "compare",
        ]);
        assert!(cli.is_err());
    }

    // === 共通エラーケーステスト ===

    #[test]
    fn test_no_subcommand_returns_error() {
        let cli = Cli::try_parse_from(["sip-load-test"]);
        assert!(cli.is_err());
    }

    #[test]
    fn test_invalid_subcommand_returns_error() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "unknown-command",
        ]);
        assert!(cli.is_err());
    }

    // === output long flag テスト ===

    #[test]
    fn test_generate_users_output_long_flag() {
        let cli = Cli::try_parse_from([
            "sip-load-test",
            "generate-users",
            "--count", "10",
            "--domain", "example.com",
            "--output", "users.json",
        ]);
        assert!(cli.is_ok());
        match cli.unwrap() {
            Cli::GenerateUsers { output, .. } => {
                assert_eq!(output, PathBuf::from("users.json"));
            }
            _ => panic!("Expected GenerateUsers"),
        }
    }

    // === run_generate_users テスト ===

    use tempfile::TempDir;
    use crate::user_pool::UsersFile;

    #[test]
    fn test_run_generate_users_creates_file_with_correct_count() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("users.json");

        let result = run_generate_users("user", 1, 5, "example.com", "pass{index}", &output, false);
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&output).unwrap();
        let users_file: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(users_file.users.len(), 5);
    }

    #[test]
    fn test_run_generate_users_append_true_appends_to_existing() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("users.json");

        // First: create file with 3 users
        run_generate_users("user", 1, 3, "example.com", "pass{index}", &output, false).unwrap();

        // Then: append 2 more users
        run_generate_users("user", 4, 2, "example.com", "pass{index}", &output, true).unwrap();

        let content = std::fs::read_to_string(&output).unwrap();
        let users_file: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(users_file.users.len(), 5);
        // Original users preserved
        assert_eq!(users_file.users[0].username, "user0001");
        assert_eq!(users_file.users[2].username, "user0003");
        // New users appended
        assert_eq!(users_file.users[3].username, "user0004");
        assert_eq!(users_file.users[4].username, "user0005");
    }

    #[test]
    fn test_run_generate_users_uses_correct_prefix_domain_password() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("users.json");

        run_generate_users("sip", 10, 2, "test.local", "secret{index}", &output, false).unwrap();

        let content = std::fs::read_to_string(&output).unwrap();
        let users_file: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(users_file.users[0].username, "sip0010");
        assert_eq!(users_file.users[0].domain, "test.local");
        assert_eq!(users_file.users[0].password, "secret0010");
        assert_eq!(users_file.users[1].username, "sip0011");
        assert_eq!(users_file.users[1].password, "secret0011");
    }

    #[test]
    fn test_run_generate_users_count_zero_creates_empty_file() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("users.json");

        let result = run_generate_users("user", 1, 0, "example.com", "pass{index}", &output, false);
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&output).unwrap();
        let users_file: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(users_file.users.len(), 0);
    }

    // === run_compare テスト ===

    use crate::reporter::ExperimentResult;
    use crate::config::Config;
    use std::collections::HashMap;

    /// テスト用のExperimentResultを生成するヘルパー
    fn make_test_result(cps: f64, total: u64, success: u64, failed: u64) -> ExperimentResult {
        ExperimentResult {
            config: Config::default(),
            total_calls: total,
            successful_calls: success,
            failed_calls: failed,
            auth_failures: 0,
            max_stable_cps: cps,
            latency_p50_ms: 5.0,
            latency_p90_ms: 10.0,
            latency_p95_ms: 15.0,
            latency_p99_ms: 20.0,
            status_codes: HashMap::new(),
            started_at: "2024-01-01T00:00:00Z".to_string(),
            finished_at: "2024-01-01T00:01:00Z".to_string(),
            steps: vec![],
        }
    }

    #[test]
    fn test_run_compare_with_valid_json_files_returns_ok() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("current.json");
        let previous_path = dir.path().join("previous.json");

        let current = make_test_result(120.0, 1000, 950, 50);
        let previous = make_test_result(100.0, 1000, 900, 100);

        std::fs::write(&current_path, serde_json::to_string_pretty(&current).unwrap()).unwrap();
        std::fs::write(&previous_path, serde_json::to_string_pretty(&previous).unwrap()).unwrap();

        let result = run_compare(&current_path, &previous_path);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_compare_with_nonexistent_current_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("nonexistent.json");
        let previous_path = dir.path().join("previous.json");

        let previous = make_test_result(100.0, 1000, 900, 100);
        std::fs::write(&previous_path, serde_json::to_string_pretty(&previous).unwrap()).unwrap();

        let result = run_compare(&current_path, &previous_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_compare_with_nonexistent_previous_file_returns_error() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("current.json");
        let previous_path = dir.path().join("nonexistent.json");

        let current = make_test_result(120.0, 1000, 950, 50);
        std::fs::write(&current_path, serde_json::to_string_pretty(&current).unwrap()).unwrap();

        let result = run_compare(&current_path, &previous_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_compare_with_invalid_json_returns_error() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("current.json");
        let previous_path = dir.path().join("previous.json");

        std::fs::write(&current_path, "not valid json").unwrap();
        std::fs::write(&previous_path, "also not valid").unwrap();

        let result = run_compare(&current_path, &previous_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_compare_with_identical_results_returns_ok() {
        let dir = TempDir::new().unwrap();
        let current_path = dir.path().join("current.json");
        let previous_path = dir.path().join("previous.json");

        let result_data = make_test_result(100.0, 1000, 950, 50);
        let json = serde_json::to_string_pretty(&result_data).unwrap();
        std::fs::write(&current_path, &json).unwrap();
        std::fs::write(&previous_path, &json).unwrap();

        let result = run_compare(&current_path, &previous_path);
        assert!(result.is_ok());
    }
}
