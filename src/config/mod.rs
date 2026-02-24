// Configuration manager module
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::error::SipLoadTestError;

/// 実行モード
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum RunMode {
    Sustained,
    StepUp,
    BinarySearch,
}

impl Default for RunMode {
    fn default() -> Self {
        RunMode::Sustained
    }
}

/// シナリオ種別
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    Register,
    InviteBye,
}

impl Default for Scenario {
    fn default() -> Self {
        Scenario::Register
    }
}

/// 負荷パターン設定
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PatternConfig {
    RampUp { duration_secs: u64 },
    Sustained { duration_secs: u64 },
    Burst,
}

impl Default for PatternConfig {
    fn default() -> Self {
        PatternConfig::Sustained { duration_secs: 60 }
    }
}

/// ステップアップ探索設定
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepUpConfig {
    pub initial_cps: f64,
    pub max_cps: f64,
    pub step_size: f64,
    pub step_duration: u64,
    pub error_threshold: f64,
}

/// 二分探索設定
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BinarySearchConfig {
    pub initial_cps: f64,
    pub step_size: f64,
    pub step_duration: u64,
    pub error_threshold: f64,
    pub convergence_threshold: f64,
    pub cooldown_duration: u64,
}
/// メイン設定構造体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub proxy_host: String,
    pub proxy_port: u16,
    pub uac_host: String,
    pub uac_port: u16,
    pub uas_host: String,
    pub uas_port: u16,
    pub target_cps: f64,
    pub max_dialogs: usize,
    pub duration: u64,
    pub scenario: Scenario,
    pub pattern: PatternConfig,
    pub call_duration: u64,
    pub health_check_timeout: u64,
    pub health_check_retries: u32,
    pub shutdown_timeout: u64,
    pub uac_port_count: u16,
    pub uas_port_count: u16,
    pub bg_register_count: u32,
    pub users_file: Option<String>,
    pub auth_enabled: bool,
    pub auth_realm: String,
    pub mode: RunMode,
    pub step_up: Option<StepUpConfig>,
    pub binary_search: Option<BinarySearchConfig>,
    pub transaction_t1_ms: Option<u64>,
    pub transaction_t2_ms: Option<u64>,
    pub transaction_t4_ms: Option<u64>,
    pub max_transactions: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            proxy_host: "127.0.0.1".to_string(),
            proxy_port: 5060,
            uac_host: "127.0.0.1".to_string(),
            uac_port: 5070,
            uas_host: "127.0.0.1".to_string(),
            uas_port: 5080,
            target_cps: 10.0,
            max_dialogs: 10_000,
            duration: 60,
            scenario: Scenario::default(),
            pattern: PatternConfig::default(),
            call_duration: 30,
            health_check_timeout: 5,
            health_check_retries: 3,
            shutdown_timeout: 10,
            uac_port_count: 1,
            uas_port_count: 1,
            bg_register_count: 0,
            users_file: None,
            auth_enabled: false,
            auth_realm: "sip-load-test".to_string(),
            mode: RunMode::default(),
            step_up: None,
            binary_search: None,
            transaction_t1_ms: None,
            transaction_t2_ms: None,
            transaction_t4_ms: None,
            max_transactions: None,
        }
    }
}

impl Config {
    /// 設定値のバリデーション
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.target_cps <= 0.0 {
            errors.push("target_cps must be greater than 0".to_string());
        }
        if self.max_dialogs == 0 {
            errors.push("max_dialogs must be greater than 0".to_string());
        }
        if self.duration == 0 {
            errors.push("duration must be greater than 0".to_string());
        }
        if self.proxy_port == 0 {
            errors.push("proxy_port must be greater than 0".to_string());
        }
        if self.uac_port == 0 {
            errors.push("uac_port must be greater than 0".to_string());
        }
        if self.uas_port == 0 {
            errors.push("uas_port must be greater than 0".to_string());
        }

        // StepUpモード時はstep_up設定が必須
        if self.mode == RunMode::StepUp {
            match &self.step_up {
                None => {
                    errors.push("step_up config is required when mode is step_up".to_string());
                }
                Some(su) => {
                    if su.initial_cps <= 0.0 {
                        errors.push("step_up.initial_cps must be greater than 0".to_string());
                    }
                    if su.max_cps < su.initial_cps {
                        errors.push("step_up.max_cps must be >= step_up.initial_cps".to_string());
                    }
                    if su.step_size <= 0.0 {
                        errors.push("step_up.step_size must be greater than 0".to_string());
                    }
                }
            }
        }

        // BinarySearchモード時はbinary_search設定が必須
        if self.mode == RunMode::BinarySearch {
            match &self.binary_search {
                None => {
                    errors.push(
                        "binary_search config is required when mode is binary_search".to_string(),
                    );
                }
                Some(bs) => {
                    if bs.initial_cps <= 0.0 {
                        errors.push("binary_search.initial_cps must be greater than 0".to_string());
                    }
                    if bs.convergence_threshold <= 0.0 {
                        errors.push(
                            "binary_search.convergence_threshold must be greater than 0"
                                .to_string(),
                        );
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// JSON文字列から設定を読み込み、バリデーションを実行する
pub fn load_from_str(json: &str) -> Result<Config, SipLoadTestError> {
    let config: Config = serde_json::from_str(json)
        .map_err(|e| SipLoadTestError::ConfigError(format!("JSON parse error: {}", e)))?;

    config.validate().map_err(|errors| {
        SipLoadTestError::ConfigError(format!("Validation errors: {}", errors.join("; ")))
    })?;

    Ok(config)
}

/// JSONファイルから設定を読み込み、バリデーションを実行する
pub fn load_from_file(path: &Path) -> Result<Config, SipLoadTestError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        SipLoadTestError::ConfigError(format!(
            "Failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;

    load_from_str(&content)
}

/// プロキシバイナリ専用の設定構造体
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyServerConfig {
    pub host: String,
    pub port: u16,
    pub forward_host: String,
    pub forward_port: u16,
    pub auth_enabled: bool,
    pub auth_realm: String,
    pub users_file: Option<String>,
    pub debug: bool,
}

impl Default for ProxyServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5060,
            forward_host: "127.0.0.1".to_string(),
            forward_port: 5080,
            auth_enabled: false,
            auth_realm: "sip-proxy".to_string(),
            users_file: None,
            debug: false,
        }
    }
}

impl ProxyServerConfig {
    /// 設定値のバリデーション
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        if self.port == 0 {
            errors.push("port must be greater than 0".to_string());
        }
        if self.forward_port == 0 {
            errors.push("forward_port must be greater than 0".to_string());
        }
        if self.auth_enabled {
            if self.users_file.is_none() {
                errors.push("users_file is required when auth_enabled is true".to_string());
            }
            if self.auth_realm.is_empty() {
                errors.push("auth_realm must not be empty when auth_enabled is true".to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// JSON文字列からプロキシ設定を読み込み、バリデーションを実行する
pub fn load_proxy_config_from_str(json: &str) -> Result<ProxyServerConfig, SipLoadTestError> {
    let config: ProxyServerConfig = serde_json::from_str(json)
        .map_err(|e| SipLoadTestError::ConfigError(format!("JSON parse error: {}", e)))?;

    config.validate().map_err(|errors| {
        SipLoadTestError::ConfigError(format!("Validation errors: {}", errors.join("; ")))
    })?;

    Ok(config)
}

/// JSONファイルからプロキシ設定を読み込み、バリデーションを実行する
pub fn load_proxy_config_from_file(path: &Path) -> Result<ProxyServerConfig, SipLoadTestError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        SipLoadTestError::ConfigError(format!(
            "Failed to read config file '{}': {}",
            path.display(),
            e
        ))
    })?;

    load_proxy_config_from_str(&content)
}

// Feature: sip-load-tester, Property 13: 設定のラウンドトリップ
#[cfg(test)]
pub mod generators {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating RunMode
    pub fn arb_run_mode() -> impl Strategy<Value = RunMode> {
        prop_oneof![
            Just(RunMode::Sustained),
            Just(RunMode::StepUp),
            Just(RunMode::BinarySearch),
        ]
    }

    /// Strategy for generating Scenario
    pub fn arb_scenario() -> impl Strategy<Value = Scenario> {
        prop_oneof![Just(Scenario::Register), Just(Scenario::InviteBye),]
    }

    /// Strategy for generating PatternConfig
    pub fn arb_pattern_config() -> impl Strategy<Value = PatternConfig> {
        prop_oneof![
            (1u64..3600).prop_map(|d| PatternConfig::RampUp { duration_secs: d }),
            (1u64..3600).prop_map(|d| PatternConfig::Sustained { duration_secs: d }),
            Just(PatternConfig::Burst),
        ]
    }

    /// Strategy for generating valid StepUpConfig
    /// Constraints: initial_cps > 0, max_cps >= initial_cps, step_size > 0
    pub fn arb_step_up_config() -> impl Strategy<Value = StepUpConfig> {
        (
            0.1f64..500.0,   // initial_cps > 0
            0.0f64..500.0,   // extra for max_cps
            0.1f64..100.0,   // step_size > 0
            1u64..3600,      // step_duration
            0.01f64..1.0,    // error_threshold
        )
            .prop_map(|(initial_cps, extra, step_size, step_duration, error_threshold)| {
                StepUpConfig {
                    initial_cps,
                    max_cps: initial_cps + extra, // ensures max_cps >= initial_cps
                    step_size,
                    step_duration,
                    error_threshold,
                }
            })
    }

    /// Strategy for generating valid BinarySearchConfig
    pub fn arb_binary_search_config() -> impl Strategy<Value = BinarySearchConfig> {
        (
            0.1f64..500.0,   // initial_cps > 0
            0.1f64..100.0,   // step_size > 0
            1u64..3600,      // step_duration
            0.01f64..1.0,    // error_threshold
            0.1f64..50.0,    // convergence_threshold > 0
            0u64..60,        // cooldown_duration
        )
            .prop_map(
                |(
                    initial_cps,
                    step_size,
                    step_duration,
                    error_threshold,
                    convergence_threshold,
                    cooldown_duration,
                )| {
                    BinarySearchConfig {
                        initial_cps,
                        step_size,
                        step_duration,
                        error_threshold,
                        convergence_threshold,
                        cooldown_duration,
                    }
                },
            )
    }

    /// Strategy for generating valid Config that passes validation.
    /// When mode is StepUp, step_up is Some; when BinarySearch, binary_search is Some;
    /// when Sustained, both can be None.
    pub fn arb_config() -> impl Strategy<Value = Config> {
        // Group 1: network settings (6 fields)
        let network = (
            "[a-z0-9]{1,8}(\\.[a-z0-9]{1,8}){0,3}", // proxy_host
            1u16..=65534,                             // proxy_port
            "[a-z0-9]{1,8}(\\.[a-z0-9]{1,8}){0,3}", // uac_host
            1u16..=65534,                             // uac_port
            "[a-z0-9]{1,8}(\\.[a-z0-9]{1,8}){0,3}", // uas_host
            1u16..=65534,                             // uas_port
        );

        // Group 2: core settings (5 fields)
        let core = (
            0.1f64..1000.0,  // target_cps > 0
            1usize..100000,  // max_dialogs > 0
            1u64..3600,      // duration > 0
            arb_scenario(),
            arb_pattern_config(),
        );

        // Group 3: timing/count settings (7 fields)
        let timing = (
            1u64..3600,      // call_duration
            1u64..60,        // health_check_timeout
            1u32..10,        // health_check_retries
            1u64..120,       // shutdown_timeout
            1u16..=100,      // uac_port_count
            1u16..=100,      // uas_port_count
            0u32..1000,      // bg_register_count
        );

        // Group 4: optional/auth settings (3 fields)
        let optional = (
            proptest::option::of("[a-z]{1,10}\\.json"),
            any::<bool>(),           // auth_enabled
            "[a-z][a-z0-9-]{0,15}",  // auth_realm
        );

        // Combine all groups with mode, then generate mode-dependent fields
        (network, core, timing, optional, arb_run_mode()).prop_flat_map(
            |(net, core, timing, opt, mode)| {
                let step_up_strategy: BoxedStrategy<Option<StepUpConfig>> = match mode {
                    RunMode::StepUp => arb_step_up_config().prop_map(Some).boxed(),
                    _ => Just(None).boxed(),
                };
                let binary_search_strategy: BoxedStrategy<Option<BinarySearchConfig>> = match mode
                {
                    RunMode::BinarySearch => arb_binary_search_config().prop_map(Some).boxed(),
                    _ => Just(None).boxed(),
                };

                (
                    Just(net),
                    Just(core),
                    Just(timing),
                    Just(opt),
                    Just(mode),
                    step_up_strategy,
                    binary_search_strategy,
                )
            },
        )
        .prop_flat_map(|(net, core, timing, opt, mode, step_up, binary_search)| {
            // Transaction config fields
            let tx_t1 = proptest::option::of(100u64..2000);
            let tx_t2 = proptest::option::of(1000u64..10000);
            let tx_t4 = proptest::option::of(1000u64..10000);
            let tx_max = proptest::option::of(1usize..100000);
            (
                Just(net),
                Just(core),
                Just(timing),
                Just(opt),
                Just(mode),
                Just(step_up),
                Just(binary_search),
                tx_t1,
                tx_t2,
                tx_t4,
                tx_max,
            )
        })
        .prop_map(|(net, core, timing, opt, mode, step_up, binary_search, tx_t1, tx_t2, tx_t4, tx_max)| Config {
            proxy_host: net.0,
            proxy_port: net.1,
            uac_host: net.2,
            uac_port: net.3,
            uas_host: net.4,
            uas_port: net.5,
            target_cps: core.0,
            max_dialogs: core.1,
            duration: core.2,
            scenario: core.3,
            pattern: core.4,
            call_duration: timing.0,
            health_check_timeout: timing.1,
            health_check_retries: timing.2,
            shutdown_timeout: timing.3,
            uac_port_count: timing.4,
            uas_port_count: timing.5,
            bg_register_count: timing.6,
            users_file: opt.0,
            auth_enabled: opt.1,
            auth_realm: opt.2,
            mode,
            step_up,
            binary_search,
            transaction_t1_ms: tx_t1,
            transaction_t2_ms: tx_t2,
            transaction_t4_ms: tx_t4,
            max_transactions: tx_max,
        })
    }
    /// Strategy for generating valid ProxyServerConfig
    /// Constraints: port > 0, forward_port > 0, and when auth_enabled=true,
    /// users_file is Some and auth_realm is non-empty.
    pub fn arb_proxy_server_config() -> impl Strategy<Value = ProxyServerConfig> {
        let base = (
            "[a-z0-9]{1,8}(\\.[a-z0-9]{1,8}){0,3}", // host
            1u16..=65534,                              // port > 0
            "[a-z0-9]{1,8}(\\.[a-z0-9]{1,8}){0,3}", // forward_host
            1u16..=65534,                              // forward_port > 0
            any::<bool>(),                             // auth_enabled
        );

        base.prop_flat_map(|(host, port, forward_host, forward_port, auth_enabled)| {
            let auth_realm_strategy: BoxedStrategy<String> = if auth_enabled {
                "[a-z][a-z0-9-]{0,15}".boxed() // non-empty when auth_enabled
            } else {
                prop_oneof![
                    Just("".to_string()),
                    "[a-z][a-z0-9-]{0,15}".prop_map(|s| s).boxed(),
                ]
                .boxed()
            };

            let users_file_strategy: BoxedStrategy<Option<String>> = if auth_enabled {
                "[a-z]{1,10}\\.json".prop_map(|s| Some(s)).boxed() // Some when auth_enabled
            } else {
                proptest::option::of("[a-z]{1,10}\\.json").boxed()
            };

            (
                Just(host),
                Just(port),
                Just(forward_host),
                Just(forward_port),
                Just(auth_enabled),
                auth_realm_strategy,
                users_file_strategy,
            )
        })
        .prop_flat_map(
            |(host, port, forward_host, forward_port, auth_enabled, auth_realm, users_file)| {
                (
                    Just(host),
                    Just(port),
                    Just(forward_host),
                    Just(forward_port),
                    Just(auth_enabled),
                    Just(auth_realm),
                    Just(users_file),
                    any::<bool>(), // debug
                )
            },
        )
        .prop_map(
            |(host, port, forward_host, forward_port, auth_enabled, auth_realm, users_file, debug)| {
                ProxyServerConfig {
                    host,
                    port,
                    forward_host,
                    forward_port,
                    auth_enabled,
                    auth_realm,
                    users_file,
                    debug,
                }
            },
        )
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ===== 設定マネージャテスト（load_from_str / load_from_file） =====

    #[test]
    fn test_load_from_str_valid_full_config() {
        let config = Config::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let loaded = load_from_str(&json).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn test_load_from_str_empty_object_uses_defaults() {
        let loaded = load_from_str("{}").unwrap();
        let default_config = Config::default();
        assert_eq!(loaded, default_config);
    }

    #[test]
    fn test_load_from_str_partial_config_uses_defaults() {
        let json = r#"{"target_cps": 50.0, "duration": 120}"#;
        let loaded = load_from_str(json).unwrap();
        assert_eq!(loaded.target_cps, 50.0);
        assert_eq!(loaded.duration, 120);
        // Other fields should be defaults
        assert_eq!(loaded.proxy_host, "127.0.0.1");
        assert_eq!(loaded.proxy_port, 5060);
        assert_eq!(loaded.max_dialogs, 10_000);
        assert_eq!(loaded.mode, RunMode::Sustained);
    }

    #[test]
    fn test_load_from_str_invalid_json() {
        let result = load_from_str("not valid json");
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            SipLoadTestError::ConfigError(msg) => {
                assert!(!msg.is_empty());
            }
            _ => panic!("Expected ConfigError, got {:?}", err),
        }
    }

    #[test]
    fn test_load_from_str_invalid_values_returns_validation_error() {
        let json = r#"{"target_cps": 0.0, "max_dialogs": 0}"#;
        let result = load_from_str(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("target_cps"));
                assert!(msg.contains("max_dialogs"));
            }
            _ => panic!("Expected ConfigError, got {:?}", err),
        }
    }

    #[test]
    fn test_load_from_str_step_up_mode_without_config() {
        let json = r#"{"mode": "step_up"}"#;
        let result = load_from_str(json);
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("step_up"));
            }
            _ => panic!("Expected ConfigError, got {:?}", err),
        }
    }

    /// 後方互換性: builtin_proxy キーを含むJSONが正常にパースされることの確認
    /// builtin_proxy フィールドが除去された後も、設定ファイルに builtin_proxy が含まれていてもエラーにならない
    /// Requirements: 4.3
    #[test]
    fn test_config_backward_compat_builtin_proxy_ignored() {
        let json = r#"{
            "builtin_proxy": {
                "enabled": true,
                "host": "192.168.1.1",
                "port": 5061,
                "auth_enabled": true,
                "auth_realm": "test-realm"
            }
        }"#;
        // builtin_proxy キーが存在してもパースが成功すること
        let loaded = load_from_str(json).unwrap();
        // パース後のConfigはデフォルト値を持つこと（builtin_proxyは無視される）
        let default_config = Config::default();
        assert_eq!(loaded.proxy_host, default_config.proxy_host);
        assert_eq!(loaded.proxy_port, default_config.proxy_port);
    }

    #[test]
    fn test_load_from_file_valid_config() {
        let config = Config::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(json.as_bytes()).unwrap();
        tmpfile.flush().unwrap();

        let loaded = load_from_file(tmpfile.path()).unwrap();
        assert_eq!(loaded, config);
    }

    #[test]
    fn test_load_from_file_nonexistent_file() {
        let result = load_from_file(Path::new("/tmp/nonexistent_config_12345.json"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        match &err {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("read") || msg.contains("No such file") || msg.contains("ファイル"));
            }
            _ => panic!("Expected ConfigError, got {:?}", err),
        }
    }

    #[test]
    fn test_load_from_file_invalid_json() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(b"{ broken json").unwrap();
        tmpfile.flush().unwrap();

        let result = load_from_file(tmpfile.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(_) => {}
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_load_from_file_empty_object_uses_defaults() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(b"{}").unwrap();
        tmpfile.flush().unwrap();

        let loaded = load_from_file(tmpfile.path()).unwrap();
        assert_eq!(loaded, Config::default());
    }

    #[test]
    fn test_load_from_file_validation_error() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(br#"{"target_cps": -1.0}"#).unwrap();
        tmpfile.flush().unwrap();

        let result = load_from_file(tmpfile.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("target_cps"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    // ===== Default値テスト =====

    #[test]
    fn test_config_default_values() {
        let config = Config::default();
        assert_eq!(config.proxy_host, "127.0.0.1");
        assert_eq!(config.proxy_port, 5060);
        assert_eq!(config.uac_host, "127.0.0.1");
        assert_eq!(config.uac_port, 5070);
        assert_eq!(config.uas_host, "127.0.0.1");
        assert_eq!(config.uas_port, 5080);
        assert_eq!(config.target_cps, 10.0);
        assert_eq!(config.max_dialogs, 10_000);
        assert_eq!(config.duration, 60);
        assert_eq!(config.scenario, Scenario::Register);
        assert_eq!(config.pattern, PatternConfig::Sustained { duration_secs: 60 });
        assert_eq!(config.call_duration, 30);
        assert_eq!(config.mode, RunMode::Sustained);
        assert!(config.step_up.is_none());
        assert!(config.binary_search.is_none());
        assert!(config.users_file.is_none());
        assert!(!config.auth_enabled);
    }

    #[test]
    fn test_run_mode_default() {
        assert_eq!(RunMode::default(), RunMode::Sustained);
    }

    #[test]
    fn test_scenario_default() {
        assert_eq!(Scenario::default(), Scenario::Register);
    }

    #[test]
    fn test_pattern_config_default() {
        assert_eq!(PatternConfig::default(), PatternConfig::Sustained { duration_secs: 60 });
    }

    // ===== バリデーションテスト =====

    #[test]
    fn test_validate_default_config_is_valid() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_target_cps_zero() {
        let mut config = Config::default();
        config.target_cps = 0.0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("target_cps")));
    }

    #[test]
    fn test_validate_target_cps_negative() {
        let mut config = Config::default();
        config.target_cps = -1.0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("target_cps")));
    }

    #[test]
    fn test_validate_max_dialogs_zero() {
        let mut config = Config::default();
        config.max_dialogs = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("max_dialogs")));
    }

    #[test]
    fn test_validate_duration_zero() {
        let mut config = Config::default();
        config.duration = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("duration")));
    }

    #[test]
    fn test_validate_port_zero() {
        let mut config = Config::default();
        config.proxy_port = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("proxy_port")));
    }

    #[test]
    fn test_validate_uac_port_zero() {
        let mut config = Config::default();
        config.uac_port = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("uac_port")));
    }

    #[test]
    fn test_validate_uas_port_zero() {
        let mut config = Config::default();
        config.uas_port = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("uas_port")));
    }

    #[test]
    fn test_validate_step_up_mode_requires_config() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = None;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("step_up config is required")));
    }

    #[test]
    fn test_validate_step_up_initial_cps_positive() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = Some(StepUpConfig {
            initial_cps: 0.0,
            max_cps: 100.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
        });
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("step_up.initial_cps")));
    }

    #[test]
    fn test_validate_step_up_max_cps_gte_initial() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = Some(StepUpConfig {
            initial_cps: 50.0,
            max_cps: 10.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
        });
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("step_up.max_cps")));
    }

    #[test]
    fn test_validate_step_up_step_size_positive() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = Some(StepUpConfig {
            initial_cps: 10.0,
            max_cps: 100.0,
            step_size: 0.0,
            step_duration: 30,
            error_threshold: 0.05,
        });
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("step_up.step_size")));
    }

    #[test]
    fn test_validate_step_up_valid_config() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = Some(StepUpConfig {
            initial_cps: 10.0,
            max_cps: 100.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
        });
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_binary_search_mode_requires_config() {
        let mut config = Config::default();
        config.mode = RunMode::BinarySearch;
        config.binary_search = None;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("binary_search config is required")));
    }

    #[test]
    fn test_validate_binary_search_initial_cps_positive() {
        let mut config = Config::default();
        config.mode = RunMode::BinarySearch;
        config.binary_search = Some(BinarySearchConfig {
            initial_cps: 0.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
            convergence_threshold: 1.0,
            cooldown_duration: 5,
        });
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("binary_search.initial_cps")));
    }

    #[test]
    fn test_validate_binary_search_convergence_threshold_positive() {
        let mut config = Config::default();
        config.mode = RunMode::BinarySearch;
        config.binary_search = Some(BinarySearchConfig {
            initial_cps: 10.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
            convergence_threshold: 0.0,
            cooldown_duration: 5,
        });
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("binary_search.convergence_threshold")));
    }

    #[test]
    fn test_validate_binary_search_valid_config() {
        let mut config = Config::default();
        config.mode = RunMode::BinarySearch;
        config.binary_search = Some(BinarySearchConfig {
            initial_cps: 10.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
            convergence_threshold: 1.0,
            cooldown_duration: 5,
        });
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_multiple_errors() {
        let mut config = Config::default();
        config.target_cps = 0.0;
        config.max_dialogs = 0;
        config.duration = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.len() >= 3);
    }

    // ===== Serdeシリアライズ/デシリアライズテスト =====

    #[test]
    fn test_config_serde_roundtrip() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_with_step_up_serde_roundtrip() {
        let mut config = Config::default();
        config.mode = RunMode::StepUp;
        config.step_up = Some(StepUpConfig {
            initial_cps: 10.0,
            max_cps: 100.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
        });
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_with_binary_search_serde_roundtrip() {
        let mut config = Config::default();
        config.mode = RunMode::BinarySearch;
        config.binary_search = Some(BinarySearchConfig {
            initial_cps: 10.0,
            step_size: 10.0,
            step_duration: 30,
            error_threshold: 0.05,
            convergence_threshold: 1.0,
            cooldown_duration: 5,
        });
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_run_mode_serde_snake_case() {
        let json = serde_json::to_string(&RunMode::StepUp).unwrap();
        assert_eq!(json, "\"step_up\"");
        let json = serde_json::to_string(&RunMode::BinarySearch).unwrap();
        assert_eq!(json, "\"binary_search\"");
        let json = serde_json::to_string(&RunMode::Sustained).unwrap();
        assert_eq!(json, "\"sustained\"");
    }

    #[test]
    fn test_scenario_serde_snake_case() {
        let json = serde_json::to_string(&Scenario::Register).unwrap();
        assert_eq!(json, "\"register\"");
        let json = serde_json::to_string(&Scenario::InviteBye).unwrap();
        assert_eq!(json, "\"invite_bye\"");
    }

    #[test]
    fn test_pattern_config_serde_tagged() {
        let pattern = PatternConfig::RampUp { duration_secs: 30 };
        let json = serde_json::to_string(&pattern).unwrap();
        assert!(json.contains("\"type\":\"ramp_up\""));
        let deserialized: PatternConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(pattern, deserialized);

        let pattern = PatternConfig::Burst;
        let json = serde_json::to_string(&pattern).unwrap();
        assert!(json.contains("\"type\":\"burst\""));
        let deserialized: PatternConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(pattern, deserialized);
    }

    #[test]
    fn test_config_with_users_file_serde_roundtrip() {
        let mut config = Config::default();
        config.users_file = Some("users.json".to_string());
        config.auth_enabled = true;
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    // ===== Property 13: 設定のラウンドトリップ =====
    // Feature: sip-load-tester, Property 13: 設定のラウンドトリップ
    // **Validates: Requirements 14.7**

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_config_roundtrip(config in generators::arb_config()) {
            let json = serde_json::to_string(&config).unwrap();
            let deserialized: Config = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(config, deserialized);
        }
    }

    // ===== Property 14: 不正設定値のエラー検出 =====
    // Feature: sip-load-tester, Property 14: 不正設定値のエラー検出
    // **Validates: Requirements 14.3**

    /// Strategy for generating non-positive f64 values (negative or zero)
    fn arb_non_positive_f64() -> BoxedStrategy<f64> {
        prop_oneof![
            (-1e6f64..0.0f64),
            Just(0.0f64),
        ]
        .boxed()
    }

    proptest! {
        /// Strategy 1: Config with target_cps <= 0 should fail validation
        #[test]
        fn prop_invalid_target_cps_fails_validation(
            mut config in generators::arb_config(),
            bad_cps in arb_non_positive_f64()
        ) {
            config.target_cps = bad_cps;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 2: Config with max_dialogs == 0 should fail validation
        #[test]
        fn prop_invalid_max_dialogs_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.max_dialogs = 0;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 3: Config with duration == 0 should fail validation
        #[test]
        fn prop_invalid_duration_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.duration = 0;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 4: Config with any port == 0 should fail validation
        #[test]
        fn prop_invalid_proxy_port_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.proxy_port = 0;
            prop_assert!(config.validate().is_err());
        }

        #[test]
        fn prop_invalid_uac_port_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.uac_port = 0;
            prop_assert!(config.validate().is_err());
        }

        #[test]
        fn prop_invalid_uas_port_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.uas_port = 0;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 5: Config with mode=StepUp but step_up=None should fail validation
        #[test]
        fn prop_step_up_mode_without_config_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.mode = RunMode::StepUp;
            config.step_up = None;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 6: Config with mode=BinarySearch but binary_search=None should fail validation
        #[test]
        fn prop_binary_search_mode_without_config_fails_validation(
            mut config in generators::arb_config()
        ) {
            config.mode = RunMode::BinarySearch;
            config.binary_search = None;
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 7: Config with mode=StepUp and step_up.initial_cps <= 0 should fail validation
        #[test]
        fn prop_step_up_invalid_initial_cps_fails_validation(
            mut config in generators::arb_config(),
            step_up in generators::arb_step_up_config(),
            bad_cps in arb_non_positive_f64()
        ) {
            config.mode = RunMode::StepUp;
            config.step_up = Some(StepUpConfig {
                initial_cps: bad_cps,
                ..step_up
            });
            prop_assert!(config.validate().is_err());
        }

        /// Strategy 8: Config with mode=BinarySearch and binary_search.convergence_threshold <= 0 should fail validation
        #[test]
        fn prop_binary_search_invalid_convergence_threshold_fails_validation(
            mut config in generators::arb_config(),
            bs in generators::arb_binary_search_config(),
            bad_threshold in arb_non_positive_f64()
        ) {
            config.mode = RunMode::BinarySearch;
            config.binary_search = Some(BinarySearchConfig {
                convergence_threshold: bad_threshold,
                ..bs
            });
            prop_assert!(config.validate().is_err());
        }
    }

    // ===== Config builtin_proxy 除去テスト (Task 3.1) =====
    // Requirements: 4.1, 4.2, 4.3, 4.4

    /// Config::default() をシリアライズした結果に "builtin_proxy" が含まれないことの確認
    /// builtin_proxy フィールドが Config から除去されていることを検証する
    /// Requirements: 4.1
    #[test]
    fn test_config_no_builtin_proxy_field() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("builtin_proxy"),
            "Config should not contain builtin_proxy field, but serialized JSON was: {}",
            json
        );
    }

    /// proxy_host と proxy_port が Config に維持されていることの確認
    /// 外部プロキシの接続先として引き続き使用される
    /// Requirements: 4.2
    #[test]
    fn test_config_proxy_host_and_port_maintained() {
        // デフォルト値の確認
        let config = Config::default();
        assert_eq!(config.proxy_host, "127.0.0.1");
        assert_eq!(config.proxy_port, 5060);

        // JSONからのパースで proxy_host/proxy_port が正しく読み込まれること
        let json = r#"{
            "proxy_host": "10.0.0.1",
            "proxy_port": 5061
        }"#;
        let loaded = load_from_str(json).unwrap();
        assert_eq!(loaded.proxy_host, "10.0.0.1");
        assert_eq!(loaded.proxy_port, 5061);

        // シリアライズ→デシリアライズのラウンドトリップ
        let mut config = Config::default();
        config.proxy_host = "192.168.1.100".to_string();
        config.proxy_port = 5070;
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.proxy_host, "192.168.1.100");
        assert_eq!(deserialized.proxy_port, 5070);
    }

    /// builtin_proxy を含むJSONに他のフィールドも含まれている場合、
    /// builtin_proxy 以外のフィールドは正しくパースされることの確認
    /// Requirements: 4.3
    #[test]
    fn test_config_backward_compat_builtin_proxy_with_other_fields() {
        let json = r#"{
            "proxy_host": "10.0.0.1",
            "proxy_port": 5061,
            "target_cps": 50.0,
            "builtin_proxy": {
                "enabled": true,
                "host": "192.168.1.1",
                "port": 5062,
                "auth_enabled": true,
                "auth_realm": "test-realm"
            }
        }"#;
        let loaded = load_from_str(json).unwrap();
        assert_eq!(loaded.proxy_host, "10.0.0.1");
        assert_eq!(loaded.proxy_port, 5061);
        assert_eq!(loaded.target_cps, 50.0);
    }

    /// Config のバリデーションに builtin_proxy 関連のルールが含まれないことの確認
    /// Requirements: 4.4
    #[test]
    fn test_config_validation_no_builtin_proxy_rules() {
        let config = Config::default();
        let result = config.validate();
        assert!(result.is_ok());
        // バリデーションエラーメッセージに builtin_proxy が含まれないこと
        let mut invalid_config = Config::default();
        invalid_config.target_cps = 0.0; // 意図的にバリデーションエラーを発生させる
        let errors = invalid_config.validate().unwrap_err();
        for error in &errors {
            assert!(
                !error.contains("builtin_proxy"),
                "Validation error should not reference builtin_proxy: {}",
                error
            );
        }
    }

    // ===== ProxyServerConfig テスト (Task 1.1) =====
    // Requirements: 2.1, 2.2, 2.3

    // --- デフォルト値テスト ---

    #[test]
    fn test_proxy_server_config_default_values() {
        let config = ProxyServerConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 5060);
        assert_eq!(config.forward_host, "127.0.0.1");
        assert_eq!(config.forward_port, 5080);
        assert!(!config.auth_enabled);
        assert_eq!(config.auth_realm, "sip-proxy");
        assert!(config.users_file.is_none());
    }

    // --- debug フィールドのユニットテスト ---

    #[test]
    fn test_proxy_server_config_debug_default_is_false() {
        let config = ProxyServerConfig::default();
        assert!(!config.debug);
    }

    #[test]
    fn test_proxy_server_config_debug_true_deserialize() {
        let json = r#"{"debug": true}"#;
        let config: ProxyServerConfig = serde_json::from_str(json).unwrap();
        assert!(config.debug);
    }

    #[test]
    fn test_proxy_server_config_debug_missing_defaults_to_false() {
        let json = r#"{"host": "0.0.0.0", "port": 5060}"#;
        let config: ProxyServerConfig = serde_json::from_str(json).unwrap();
        assert!(!config.debug);
    }

    // --- 有効なJSONからのパーステスト ---

    #[test]
    fn test_proxy_server_config_parse_full_json() {
        let json = r#"{
            "host": "192.168.1.1",
            "port": 5061,
            "forward_host": "10.0.0.1",
            "forward_port": 5081,
            "auth_enabled": true,
            "auth_realm": "my-realm",
            "users_file": "users.json"
        }"#;
        let config: ProxyServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.host, "192.168.1.1");
        assert_eq!(config.port, 5061);
        assert_eq!(config.forward_host, "10.0.0.1");
        assert_eq!(config.forward_port, 5081);
        assert!(config.auth_enabled);
        assert_eq!(config.auth_realm, "my-realm");
        assert_eq!(config.users_file, Some("users.json".to_string()));
    }

    #[test]
    fn test_proxy_server_config_parse_empty_object_uses_defaults() {
        let config: ProxyServerConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config, ProxyServerConfig::default());
    }

    #[test]
    fn test_proxy_server_config_parse_partial_json_uses_defaults() {
        let json = r#"{"port": 9090, "auth_enabled": true}"#;
        let config: ProxyServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.port, 9090);
        assert!(config.auth_enabled);
        // Other fields should be defaults
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.forward_host, "127.0.0.1");
        assert_eq!(config.forward_port, 5080);
        assert_eq!(config.auth_realm, "sip-proxy");
        assert!(config.users_file.is_none());
    }

    // --- バリデーションテスト ---

    #[test]
    fn test_proxy_server_config_validate_default_is_valid() {
        let config = ProxyServerConfig::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_proxy_server_config_validate_port_zero() {
        let mut config = ProxyServerConfig::default();
        config.port = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("port")));
    }

    #[test]
    fn test_proxy_server_config_validate_forward_port_zero() {
        let mut config = ProxyServerConfig::default();
        config.forward_port = 0;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("forward_port")));
    }

    #[test]
    fn test_proxy_server_config_validate_auth_enabled_without_users_file() {
        let mut config = ProxyServerConfig::default();
        config.auth_enabled = true;
        config.users_file = None;
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("users_file")));
    }

    #[test]
    fn test_proxy_server_config_validate_auth_realm_empty_when_auth_enabled() {
        let mut config = ProxyServerConfig::default();
        config.auth_enabled = true;
        config.auth_realm = "".to_string();
        config.users_file = Some("users.json".to_string());
        let errors = config.validate().unwrap_err();
        assert!(errors.iter().any(|e| e.contains("auth_realm")));
    }

    #[test]
    fn test_proxy_server_config_validate_auth_realm_empty_when_auth_disabled() {
        // auth_realm が空でも auth_enabled=false なら OK
        let mut config = ProxyServerConfig::default();
        config.auth_enabled = false;
        config.auth_realm = "".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_proxy_server_config_validate_auth_enabled_with_users_file() {
        let mut config = ProxyServerConfig::default();
        config.auth_enabled = true;
        config.auth_realm = "sip-proxy".to_string();
        config.users_file = Some("users.json".to_string());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_proxy_server_config_validate_multiple_errors() {
        let mut config = ProxyServerConfig::default();
        config.port = 0;
        config.forward_port = 0;
        config.auth_enabled = true;
        config.users_file = None;
        config.auth_realm = "".to_string();
        let errors = config.validate().unwrap_err();
        assert!(errors.len() >= 3);
    }

    // --- load_proxy_config_from_str テスト ---

    #[test]
    fn test_load_proxy_config_from_str_valid_json() {
        let json = r#"{
            "host": "0.0.0.0",
            "port": 5060,
            "forward_host": "127.0.0.1",
            "forward_port": 5080
        }"#;
        let config = load_proxy_config_from_str(json).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 5060);
        assert_eq!(config.forward_host, "127.0.0.1");
        assert_eq!(config.forward_port, 5080);
    }

    #[test]
    fn test_load_proxy_config_from_str_empty_object() {
        let config = load_proxy_config_from_str("{}").unwrap();
        assert_eq!(config, ProxyServerConfig::default());
    }

    #[test]
    fn test_load_proxy_config_from_str_invalid_json() {
        let result = load_proxy_config_from_str("not valid json");
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(!msg.is_empty());
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_load_proxy_config_from_str_validation_error() {
        let json = r#"{"port": 0}"#;
        let result = load_proxy_config_from_str(json);
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("port"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    // --- load_proxy_config_from_file テスト ---

    #[test]
    fn test_load_proxy_config_from_file_valid() {
        let json = r#"{"port": 5060, "forward_port": 5080}"#;
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(json.as_bytes()).unwrap();
        tmpfile.flush().unwrap();

        let config = load_proxy_config_from_file(tmpfile.path()).unwrap();
        assert_eq!(config.port, 5060);
        assert_eq!(config.forward_port, 5080);
    }

    #[test]
    fn test_load_proxy_config_from_file_nonexistent() {
        let result = load_proxy_config_from_file(Path::new("/tmp/nonexistent_proxy_config_99999.json"));
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(_) => {}
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_load_proxy_config_from_file_invalid_json() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(b"{ broken").unwrap();
        tmpfile.flush().unwrap();

        let result = load_proxy_config_from_file(tmpfile.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(_) => {}
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_load_proxy_config_from_file_validation_error() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(br#"{"port": 0}"#).unwrap();
        tmpfile.flush().unwrap();

        let result = load_proxy_config_from_file(tmpfile.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("port"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    #[test]
    fn test_load_proxy_config_from_file_empty_object_uses_defaults() {
        let mut tmpfile = NamedTempFile::new().unwrap();
        tmpfile.write_all(b"{}").unwrap();
        tmpfile.flush().unwrap();

        let config = load_proxy_config_from_file(tmpfile.path()).unwrap();
        assert_eq!(config, ProxyServerConfig::default());
    }

    // --- Serde ラウンドトリップテスト ---

    #[test]
    fn test_proxy_server_config_serde_roundtrip() {
        let config = ProxyServerConfig {
            host: "10.0.0.1".to_string(),
            port: 5061,
            forward_host: "10.0.0.2".to_string(),
            forward_port: 5081,
            auth_enabled: true,
            auth_realm: "test-realm".to_string(),
            users_file: Some("users.json".to_string()),
            debug: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ProxyServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    // ===== Property 1: ProxyServerConfig のラウンドトリップ =====
    // Feature: proxy-process-separation, Property 1: ProxyServerConfig のラウンドトリップ
    // **Validates: Requirements 2.3, 2.4, 2.5**
    // Feature: proxy-debug-logging, Property 6: ProxyServerConfig のラウンドトリップ
    // **Validates: Requirements 9.1**

    proptest! {
        #[test]
        fn prop_proxy_server_config_roundtrip(config in generators::arb_proxy_server_config()) {
            let json = serde_json::to_string(&config).unwrap();
            let deserialized: ProxyServerConfig = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(config, deserialized);
        }
    }

    // ===== Property 2: 不正な設定値のバリデーション拒否 =====
    // Feature: proxy-process-separation, Property 2: 不正な設定値のバリデーション拒否
    // **Validates: Requirements 2.2**

    proptest! {
        /// ProxyServerConfig with port=0 should always fail validation
        #[test]
        fn prop_proxy_server_config_port_zero_rejected(
            mut config in generators::arb_proxy_server_config()
        ) {
            config.port = 0;
            prop_assert!(config.validate().is_err());
        }

        /// ProxyServerConfig with forward_port=0 should always fail validation
        #[test]
        fn prop_proxy_server_config_forward_port_zero_rejected(
            mut config in generators::arb_proxy_server_config()
        ) {
            config.forward_port = 0;
            prop_assert!(config.validate().is_err());
        }

        /// ProxyServerConfig with auth_enabled=true and users_file=None should always fail validation
        #[test]
        fn prop_proxy_server_config_auth_without_users_file_rejected(
            mut config in generators::arb_proxy_server_config()
        ) {
            config.auth_enabled = true;
            config.users_file = None;
            // Ensure auth_realm is non-empty so only users_file triggers the error
            if config.auth_realm.is_empty() {
                config.auth_realm = "test-realm".to_string();
            }
            prop_assert!(config.validate().is_err());
        }
    }

    // ===== トランザクション設定フィールドテスト (Task 13.1) =====
    // Requirements: 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 12.4

    #[test]
    fn test_config_transaction_fields_default_to_none() {
        let config = Config::default();
        assert_eq!(config.transaction_t1_ms, None);
        assert_eq!(config.transaction_t2_ms, None);
        assert_eq!(config.transaction_t4_ms, None);
        assert_eq!(config.max_transactions, None);
    }

    #[test]
    fn test_config_transaction_fields_serde_roundtrip() {
        let mut config = Config::default();
        config.transaction_t1_ms = Some(500);
        config.transaction_t2_ms = Some(4000);
        config.transaction_t4_ms = Some(5000);
        config.max_transactions = Some(10000);
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_transaction_fields_backward_compat_missing_fields() {
        // トランザクションフィールドが存在しないJSONでもデシリアライズが成功し、Noneになること
        let json = r#"{"target_cps": 10.0}"#;
        let loaded = load_from_str(json).unwrap();
        assert_eq!(loaded.transaction_t1_ms, None);
        assert_eq!(loaded.transaction_t2_ms, None);
        assert_eq!(loaded.transaction_t4_ms, None);
        assert_eq!(loaded.max_transactions, None);
    }

    #[test]
    fn test_config_transaction_fields_partial_set() {
        // 一部のトランザクションフィールドのみ設定した場合
        let json = r#"{"transaction_t1_ms": 250, "max_transactions": 5000}"#;
        let loaded = load_from_str(json).unwrap();
        assert_eq!(loaded.transaction_t1_ms, Some(250));
        assert_eq!(loaded.transaction_t2_ms, None);
        assert_eq!(loaded.transaction_t4_ms, None);
        assert_eq!(loaded.max_transactions, Some(5000));
    }

    // ===== Property 3: 後方互換性 - builtin_proxy の無視 =====
    // Feature: proxy-process-separation, Property 3: 後方互換性 - builtin_proxy の無視
    // **Validates: Requirements 4.3**

    proptest! {
        /// 任意の有効な Config JSON に builtin_proxy フィールドを追加しても
        /// パースが成功し、builtin_proxy 以外のフィールドが正しく維持されることを検証
        #[test]
        fn prop_config_backward_compat_builtin_proxy_ignored(
            config in generators::arb_config()
        ) {
            // 1. 有効な Config を JSON Value にシリアライズ
            let mut json_value = serde_json::to_value(&config).unwrap();

            // 2. builtin_proxy フィールドを注入（既存の値を上書き）
            //    BuiltinProxyConfig 互換の JSON オブジェクトを使用
            //    Task 3.3 で builtin_proxy フィールドが除去された後も、
            //    serde(default) により未知フィールドは無視されるため成功する
            json_value.as_object_mut().unwrap().insert(
                "builtin_proxy".to_string(),
                serde_json::json!({
                    "enabled": true,
                    "host": "1.2.3.4",
                    "port": 5060,
                    "auth_enabled": false,
                    "auth_realm": "test-realm"
                }),
            );

            // 3. JSON 文字列に変換してパース
            let json_str = serde_json::to_string(&json_value).unwrap();
            let result = load_from_str(&json_str);

            // 4. パースが成功することを検証
            prop_assert!(result.is_ok(), "load_from_str failed with builtin_proxy injected: {:?}", result.err());

            // 5. パース結果が元の Config と一致することを検証（builtin_proxy 以外）
            let parsed = result.unwrap();
            prop_assert_eq!(parsed.proxy_host, config.proxy_host);
            prop_assert_eq!(parsed.proxy_port, config.proxy_port);
            prop_assert_eq!(parsed.uac_host, config.uac_host);
            prop_assert_eq!(parsed.uac_port, config.uac_port);
            prop_assert_eq!(parsed.uas_host, config.uas_host);
            prop_assert_eq!(parsed.uas_port, config.uas_port);
            prop_assert_eq!(parsed.target_cps, config.target_cps);
            prop_assert_eq!(parsed.max_dialogs, config.max_dialogs);
            prop_assert_eq!(parsed.duration, config.duration);
            prop_assert_eq!(parsed.scenario, config.scenario);
            prop_assert_eq!(parsed.call_duration, config.call_duration);
            prop_assert_eq!(parsed.mode, config.mode);
        }
    }
}
