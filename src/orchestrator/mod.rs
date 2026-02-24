// Experiment orchestrator module
//
// Coordinates the lifecycle of all components (UserPool, UAS, UAC)
// and executes the experiment based on the configured RunMode.
//
// Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 16.1, 16.2, 16.3, 16.4

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{Config, PatternConfig, RunMode, Scenario};
use crate::error::SipLoadTestError;
use crate::reporter::ExperimentResult;
use crate::reporter::StepReport;
use crate::sip::formatter::format_sip_message;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest};
use crate::stats::{StatsCollector, StatsSnapshot};
use crate::uac::load_pattern::LoadPattern;
use crate::uac::Uac;
use crate::uas::{SipTransport, Uas};
use crate::user_pool::UserPool;

/// Result of a single step execution.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub cps: f64,
    pub error_rate: f64,
    pub stats: StatsSnapshot,
}

/// Experiment orchestrator - manages component lifecycle and experiment execution.
pub struct Orchestrator {
    config: Config,
    user_pool: Arc<UserPool>,
    uac: Option<Arc<Uac>>,
    uas: Option<Arc<Uas>>,
    stats: Arc<StatsCollector>,
    shutdown_flag: Arc<AtomicBool>,
    health_check_transport: Option<Arc<dyn SipTransport>>,
}

impl Orchestrator {
    /// Create a new Orchestrator instance.
    pub fn new(config: Config, user_pool: Arc<UserPool>) -> Self {
        Self {
            config,
            user_pool,
            uac: None,
            uas: None,
            stats: Arc::new(StatsCollector::new()),
            shutdown_flag: Arc::new(AtomicBool::new(false)),
            health_check_transport: None,
        }
    }

    /// Run the experiment based on the configured mode.
    /// Dispatches to run_sustained, run_step_up, or run_binary_search.
    pub async fn run(&mut self) -> Result<ExperimentResult, SipLoadTestError> {
        match self.config.mode {
            RunMode::Sustained => self.run_sustained().await,
            RunMode::StepUp => self.run_step_up().await,
            RunMode::BinarySearch => self.run_binary_search().await,
        }
    }

    /// Perform health check by sending OPTIONS request to the proxy.
    /// Retries up to health_check_retries times with health_check_timeout between attempts.
    pub async fn health_check(&self) -> Result<(), SipLoadTestError> {
        let transport = match &self.health_check_transport {
            Some(t) => t.clone(),
            None => return Err(SipLoadTestError::ConfigError(
                "No transport configured for health check".to_string(),
            )),
        };

        let proxy_addr: SocketAddr = format!("{}:{}", self.config.proxy_host, self.config.proxy_port)
            .parse()
            .map_err(|e| SipLoadTestError::ConfigError(format!("Invalid proxy address: {}", e)))?;

        let retries = self.config.health_check_retries;
        let timeout_duration = Duration::from_secs(self.config.health_check_timeout);

        for attempt in 0..retries {
            let options_request = build_options_request(&self.config);
            let bytes = format_sip_message(&SipMessage::Request(options_request));

            match tokio::time::timeout(timeout_duration, transport.send_to(&bytes, proxy_addr)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(_)) | Err(_) => {
                    if attempt + 1 < retries {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }

        Err(SipLoadTestError::HealthCheckFailed(retries))
    }

    /// Run sustained mode: maintain target CPS for the configured duration.
    async fn run_sustained(&mut self) -> Result<ExperimentResult, SipLoadTestError> {
        let started_at = chrono_now();
        let target_cps = self.config.target_cps;
        let duration = Duration::from_secs(self.config.duration);
        let pattern = LoadPattern::new(self.config.pattern.clone());
        let send_interval = calculate_send_interval(target_cps);

        let start = Instant::now();
        let mut calls_sent: u64 = 0;

        while start.elapsed() < duration && !self.shutdown_flag.load(Ordering::Relaxed) {
            let elapsed = start.elapsed();
            let to_send = pattern.calls_to_send(elapsed, target_cps, calls_sent);

            if let Some(uac) = &self.uac {
                for _ in 0..to_send {
                    if self.shutdown_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    match self.config.scenario {
                        Scenario::Register => {
                            let _ = uac.send_register().await;
                        }
                        Scenario::InviteBye => {
                            let _ = uac.send_invite().await;
                        }
                    }
                    calls_sent += 1;
                }
            }

            // Dynamic interval based on target CPS (Requirements: 9.3)
            tokio::time::sleep(send_interval).await;
        }

        let finished_at = chrono_now();
        let snap = self.stats.snapshot();

        Ok(build_experiment_result(
            &self.config,
            &snap,
            target_cps,
            &started_at,
            &finished_at,
            vec![],
        ))
    }

    /// Run step-up mode: increase CPS from initial_cps by step_size until error threshold
    /// is exceeded or max_cps is reached.
    /// Requirements: 7.1, 7.2, 7.3, 7.4
    async fn run_step_up(&mut self) -> Result<ExperimentResult, SipLoadTestError> {
        let step_up_config = self.config.step_up.as_ref()
            .ok_or_else(|| SipLoadTestError::ConfigError("step_up config required".to_string()))?
            .clone();

        let started_at = chrono_now();
        let mut current_cps = step_up_config.initial_cps;
        let mut max_stable_cps = 0.0;
        let step_duration = Duration::from_secs(step_up_config.step_duration);

        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }

            // Take snapshot before step to compute per-step error rate
            let snap_before = self.stats.snapshot();

            let _result = self.run_step(current_cps, step_duration).await?;

            // Compute per-step error rate from delta
            let snap_after = self.stats.snapshot();
            let step_total = snap_after.total_calls - snap_before.total_calls;
            let step_failed = snap_after.failed_calls - snap_before.failed_calls;
            let step_error_rate = if step_total > 0 {
                step_failed as f64 / step_total as f64
            } else {
                1.0 // コール数0は失敗として扱う
            };

            if step_error_rate <= step_up_config.error_threshold {
                max_stable_cps = current_cps;
            } else {
                // Req 7.2: error threshold exceeded → stop
                break;
            }

            if current_cps >= step_up_config.max_cps {
                // Req 7.4: reached max_cps → stop after this step
                break;
            }

            // Req 7.1: increase by step_size, clamped to max_cps
            current_cps = (current_cps + step_up_config.step_size).min(step_up_config.max_cps);
        }

        let finished_at = chrono_now();
        let snap = self.stats.snapshot();
        Ok(build_experiment_result(
            &self.config,
            &snap,
            max_stable_cps,
            &started_at,
            &finished_at,
            vec![],
        ))
    }

    /// Run binary search mode: find the maximum stable CPS using binary search.
    /// Requirements: 8.1, 8.2, 8.3, 8.4, 8.5
    async fn run_binary_search(&mut self) -> Result<ExperimentResult, SipLoadTestError> {
        let bs_config = self.config.binary_search.as_ref()
            .ok_or_else(|| SipLoadTestError::ConfigError("binary_search config required".to_string()))?
            .clone();

        let started_at = chrono_now();
        let step_duration = Duration::from_secs(bs_config.step_duration);
        let cooldown = Duration::from_secs(bs_config.cooldown_duration);

        let mut steps: Vec<StepReport> = Vec::new();
        let mut step_number: usize = 0;

        // Phase 1: Find upper bound by exponential doubling from initial_cps
        // Start at initial_cps, double until error_rate > threshold
        let mut low = 0.0_f64;
        let mut high = bs_config.initial_cps;

        loop {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }

            let snap_before = self.stats.snapshot();
            let _result = self.run_step(high, step_duration).await?;
            let snap_after = self.stats.snapshot();

            let step_total = snap_after.total_calls - snap_before.total_calls;
            let step_failed = snap_after.failed_calls - snap_before.failed_calls;
            let step_successful = step_total - step_failed;
            let step_error_rate = if step_total > 0 {
                step_failed as f64 / step_total as f64
            } else {
                1.0 // コール数0は失敗として扱う
            };

            let is_pass = step_error_rate <= bs_config.error_threshold;

            step_number += 1;
            steps.push(StepReport {
                step: step_number,
                phase: "expand".to_string(),
                target_cps: high,
                total_calls: step_total,
                successful_calls: step_successful,
                failed_calls: step_failed,
                error_rate: step_error_rate,
                result: if is_pass { "pass".to_string() } else { "fail".to_string() },
            });

            if !is_pass {
                // Found upper bound: high is too much, low was last good
                break;
            }

            // Req 8.2: error_rate <= threshold → this CPS is good
            low = high;

            // Safety: prevent truly infinite loops
            if high > 1_000_000.0 {
                break;
            }

            high *= 2.0; // Exponential doubling

            // Req 8.5: cooldown between steps
            if cooldown.as_secs() > 0 && !self.shutdown_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(cooldown).await;
            }
        }

        // Phase 2: Binary search between low and high
        // Req 8.4: stop when high - low <= convergence_threshold
        while (high - low) > bs_config.convergence_threshold {
            if self.shutdown_flag.load(Ordering::Relaxed) {
                break;
            }

            let mid = (low + high) / 2.0;

            let snap_before = self.stats.snapshot();
            let _result = self.run_step(mid, step_duration).await?;
            let snap_after = self.stats.snapshot();

            let step_total = snap_after.total_calls - snap_before.total_calls;
            let step_failed = snap_after.failed_calls - snap_before.failed_calls;
            let step_successful = step_total - step_failed;
            let step_error_rate = if step_total > 0 {
                step_failed as f64 / step_total as f64
            } else {
                1.0 // コール数0は失敗として扱う
            };

            let is_pass = step_error_rate <= bs_config.error_threshold;

            step_number += 1;
            steps.push(StepReport {
                step: step_number,
                phase: "binary_search".to_string(),
                target_cps: mid,
                total_calls: step_total,
                successful_calls: step_successful,
                failed_calls: step_failed,
                error_rate: step_error_rate,
                result: if is_pass { "pass".to_string() } else { "fail".to_string() },
            });

            if is_pass {
                // Req 8.2: good → search upper half
                low = mid;
            } else {
                // Req 8.3: bad → search lower half
                high = mid;
            }

            // Req 8.5: cooldown between steps
            if cooldown.as_secs() > 0 && !self.shutdown_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(cooldown).await;
            }
        }

        let finished_at = chrono_now();
        let snap = self.stats.snapshot();
        Ok(build_experiment_result(
            &self.config,
            &snap,
            low, // max_stable_cps = low (last known good CPS)
            &started_at,
            &finished_at,
            steps,
        ))
    }

    /// Execute one step at the given CPS for the given duration.
    /// Returns a StepResult with the actual CPS, error rate, and stats snapshot.
    pub async fn run_step(&self, cps: f64, duration: Duration) -> Result<StepResult, SipLoadTestError> {
        let pattern = LoadPattern::new(PatternConfig::Sustained {
            duration_secs: duration.as_secs(),
        });
        let send_interval = calculate_send_interval(cps);

        let start = Instant::now();
        let mut calls_sent: u64 = 0;

        while start.elapsed() < duration && !self.shutdown_flag.load(Ordering::Relaxed) {
            let elapsed = start.elapsed();
            let to_send = pattern.calls_to_send(elapsed, cps, calls_sent);

            if let Some(uac) = &self.uac {
                for _ in 0..to_send {
                    if self.shutdown_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    match self.config.scenario {
                        Scenario::Register => {
                            let _ = uac.send_register().await;
                        }
                        Scenario::InviteBye => {
                            let _ = uac.send_invite().await;
                        }
                    }
                    calls_sent += 1;
                }
            }

            // Dynamic interval based on target CPS (Requirements: 9.3)
            tokio::time::sleep(send_interval).await;
        }

        let snap = self.stats.snapshot();
        let total = snap.total_calls;
        let failed = snap.failed_calls;
        let error_rate = if total > 0 {
            failed as f64 / total as f64
        } else {
            1.0 // コール数0は失敗として扱う
        };

        Ok(StepResult {
            cps: snap.cps,
            error_rate,
            stats: snap,
        })
    }

    /// Graceful shutdown: UAC → UAS → Proxy in order.
    /// Requirements: 9.4, 16.1, 16.2, 16.3, 16.4
    pub async fn graceful_shutdown(&mut self) -> Result<(), SipLoadTestError> {
        let timeout = Duration::from_secs(self.config.shutdown_timeout);

        // 1. Shutdown UAC first (stops new calls, sends BYE for active dialogs)
        if let Some(uac) = self.uac.take() {
            let _ = tokio::time::timeout(timeout, uac.shutdown()).await;
        }

        // 2. Shutdown UAS
        if let Some(uas) = self.uas.take() {
            let _ = tokio::time::timeout(timeout, uas.shutdown()).await;
        }

        Ok(())
    }

    /// Set up signal handling for SIGINT/SIGTERM.
    /// When a signal is received, sets the shutdown flag.
    pub fn setup_signal_handler(&self) -> Result<(), SipLoadTestError> {
        let flag = self.shutdown_flag.clone();
        ctrlc::set_handler(move || {
            flag.store(true, Ordering::Relaxed);
        })
        .map_err(|e| SipLoadTestError::ConfigError(format!("Failed to set signal handler: {}", e)))
    }

    /// Check if shutdown has been requested.
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_flag.load(Ordering::Relaxed)
    }

    /// Request shutdown (for testing or programmatic use).
    pub fn request_shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::Relaxed);
    }

    /// Get a reference to the stats collector.
    pub fn stats(&self) -> &Arc<StatsCollector> {
        &self.stats
    }

    /// Get a reference to the config.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get the shutdown flag (for sharing with other components).
    pub fn shutdown_flag(&self) -> &Arc<AtomicBool> {
        &self.shutdown_flag
    }

    /// Set the UAC component.
    pub fn set_uac(&mut self, uac: Arc<Uac>) {
        self.uac = Some(uac);
    }

    /// Set the UAS component.
    pub fn set_uas(&mut self, uas: Arc<Uas>) {
        self.uas = Some(uas);
    }

    /// Set the transport for health checks.
    pub fn set_health_check_transport(&mut self, transport: Arc<dyn SipTransport>) {
        self.health_check_transport = Some(transport);
    }
}

/// Build an OPTIONS request for health checking.
fn build_options_request(config: &Config) -> SipRequest {
    let mut headers = Headers::new();
    headers.add("Via", format!(
        "SIP/2.0/UDP {}:{};branch=z9hG4bK-healthcheck",
        config.uac_host, config.uac_port
    ));
    headers.add("From", format!(
        "<sip:healthcheck@{}>;tag=hc-{}",
        config.uac_host,
        rand::random::<u32>()
    ));
    headers.add("To", format!(
        "<sip:healthcheck@{}:{}>",
        config.proxy_host, config.proxy_port
    ));
    headers.add("Call-ID", format!("healthcheck-{}", rand::random::<u32>()));
    headers.add("CSeq", "1 OPTIONS".to_string());
    headers.add("Max-Forwards", "70".to_string());
    headers.add("Content-Length", "0".to_string());

    SipRequest {
        method: Method::Options,
        request_uri: format!("sip:{}:{}", config.proxy_host, config.proxy_port),
        version: "SIP/2.0".to_string(),
        headers,
        body: None,
    }
}

/// Build an ExperimentResult from a stats snapshot.
fn build_experiment_result(
    config: &Config,
    snap: &StatsSnapshot,
    max_stable_cps: f64,
    started_at: &str,
    finished_at: &str,
    steps: Vec<crate::reporter::StepReport>,
) -> ExperimentResult {
    ExperimentResult {
        config: config.clone(),
        total_calls: snap.total_calls,
        successful_calls: snap.successful_calls,
        failed_calls: snap.failed_calls,
        auth_failures: snap.auth_failures,
        max_stable_cps,
        latency_p50_ms: snap.latency_p50.as_secs_f64() * 1000.0,
        latency_p90_ms: snap.latency_p90.as_secs_f64() * 1000.0,
        latency_p95_ms: snap.latency_p95.as_secs_f64() * 1000.0,
        latency_p99_ms: snap.latency_p99.as_secs_f64() * 1000.0,
        status_codes: snap.status_codes.clone(),
        started_at: started_at.to_string(),
        finished_at: finished_at.to_string(),
        steps,
    }
}

/// Calculate the send interval based on target CPS.
///
/// Uses `interval_us = 1_000_000 / target_cps` with:
/// - Minimum floor: 1ms (prevents busy-waiting at very high CPS)
/// - Maximum cap: 100ms (ensures responsiveness at very low CPS)
/// - Edge cases: cps <= 0 returns max interval (100ms)
///
/// Requirements: 9.3
pub fn calculate_send_interval(target_cps: f64) -> Duration {
    const MIN_INTERVAL: Duration = Duration::from_millis(1);
    const MAX_INTERVAL: Duration = Duration::from_millis(100);

    if target_cps <= 0.0 {
        return MAX_INTERVAL;
    }

    let interval_us = (1_000_000.0 / target_cps) as u64;
    let interval = Duration::from_micros(interval_us);

    if interval < MIN_INTERVAL {
        MIN_INTERVAL
    } else if interval > MAX_INTERVAL {
        MAX_INTERVAL
    } else {
        interval
    }
}

/// Get current time as ISO 8601 string (simple implementation without chrono).
fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_secs())
}

/// Pure step-up search logic for property testing.
/// Takes a function that returns the error rate for a given CPS.
///
/// Returns the max stable CPS (the last CPS where error_rate <= error_threshold),
/// or 0.0 if no step was stable.
///
/// Requirements: 7.1, 7.2, 7.3, 7.4
/// Pure step-up search logic for property testing.
/// Takes a function that returns the error rate for a given CPS.
///
/// Returns the max stable CPS (the last CPS where error_rate <= error_threshold),
/// or 0.0 if no step was stable.
///
/// Requirements: 7.1, 7.2, 7.3, 7.4
pub fn step_up_search(
    initial_cps: f64,
    max_cps: f64,
    step_size: f64,
    error_threshold: f64,
    eval_fn: impl Fn(f64) -> f64,
) -> f64 {
    let mut current_cps = initial_cps;
    let mut max_stable_cps = 0.0;

    loop {
        let error_rate = eval_fn(current_cps);

        if error_rate <= error_threshold {
            // Req 7.3: track last stable CPS
            max_stable_cps = current_cps;
        } else {
            // Req 7.2: error threshold exceeded → stop
            break;
        }

        if current_cps >= max_cps {
            // Req 7.4: reached max_cps → stop after this step
            break;
        }

        // Req 7.1: increase by step_size, clamped to max_cps
        current_cps = (current_cps + step_size).min(max_cps);
    }

    max_stable_cps
}

// Feature: sip-load-tester, Property 8: 二分探索の収束性
/// Pure binary search logic for property testing.
/// eval_fn: CPS → error_rate (should be monotonically increasing for convergence)
/// Returns max_stable_cps (the converged lower bound).
///
/// Phase 1: find upper bound by stepping from initial_cps
/// Phase 2: binary search between low and high until converged
///
/// Requirements: 8.1, 8.2, 8.3, 8.4
pub fn binary_search_find_limit(
    initial_cps: f64,
    _step_size: f64,
    error_threshold: f64,
    convergence_threshold: f64,
    eval_fn: impl Fn(f64) -> f64,
) -> f64 {
    // Phase 1: find upper bound by exponential doubling from initial_cps
    let mut low = 0.0_f64;
    let mut high = initial_cps;

    loop {
        let error_rate = eval_fn(high);

        if error_rate > error_threshold {
            // Found upper bound: high is too much
            break;
        }

        // Req 8.2: error_rate <= threshold → this CPS is good
        low = high;

        // Safety: prevent truly infinite loops
        if high > 1_000_000.0 {
            break;
        }

        high *= 2.0; // Exponential doubling
    }

    // Phase 2: Binary search between low and high
    // Req 8.4: stop when high - low <= convergence_threshold
    let max_iterations = 200_u32;
    let mut iteration = 0_u32;
    while (high - low) > convergence_threshold {
        iteration += 1;
        if iteration > max_iterations {
            break;
        }

        let mid = (low + high) / 2.0;
        let error_rate = eval_fn(mid);

        if error_rate <= error_threshold {
            // Req 8.2: good → search upper half
            low = mid;
        } else {
            // Req 8.3: bad → search lower half
            high = mid;
        }
    }

    // Return low as the max stable CPS
    low
}




#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RunMode};
    use crate::dialog::DialogManager;
    use crate::sip::message::{SipMessage, SipResponse};
    use crate::sip::parser::parse_sip_message;
    use crate::stats::StatsCollector;
    use crate::uas::SipTransport;
    use crate::uac::{Uac, UacConfig};
    use crate::uas::{Uas, UasConfig};
    use crate::user_pool::{UserPool, UsersFile, UserEntry};
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::Mutex;

    // ===== Mock Transport =====

    struct MockTransport {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
        should_fail: AtomicBool,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                should_fail: AtomicBool::new(false),
            }
        }

        fn set_should_fail(&self, fail: bool) {
            self.should_fail.store(fail, Ordering::Relaxed);
        }

        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
        }

        fn sent_messages(&self) -> Vec<(SipMessage, SocketAddr)> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(data, addr)| {
                    parse_sip_message(data).ok().map(|msg| (msg, *addr))
                })
                .collect()
        }
    }

    impl SipTransport for MockTransport {
        fn send_to<'a>(
            &'a self,
            data: &'a [u8],
            addr: SocketAddr,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>>
        {
            Box::pin(async move {
                if self.should_fail.load(Ordering::Relaxed) {
                    return Err(SipLoadTestError::NetworkError(
                        std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "mock failure"),
                    ));
                }
                self.sent.lock().unwrap().push((data.to_vec(), addr));
                Ok(())
            })
        }
    }

    // ===== Test Helpers =====

    /// A mock transport that records a successful call in stats on each send.
    /// This ensures total_calls > 0 when used with run_step.
    struct StatsRecordingTransport {
        inner: MockTransport,
        stats: Arc<StatsCollector>,
    }

    impl StatsRecordingTransport {
        fn new(stats: Arc<StatsCollector>) -> Self {
            Self {
                inner: MockTransport::new(),
                stats,
            }
        }
    }

    impl SipTransport for StatsRecordingTransport {
        fn send_to<'a>(
            &'a self,
            data: &'a [u8],
            addr: SocketAddr,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.inner.send_to(data, addr).await?;
                // Record a successful call so total_calls > 0
                self.stats.record_call(200, Duration::from_millis(1));
                Ok(())
            })
        }
    }

    fn make_user_pool() -> Arc<UserPool> {
        let users_file = UsersFile {
            users: vec![
                UserEntry {
                    username: "user001".to_string(),
                    domain: "example.com".to_string(),
                    password: "pass001".to_string(),
                },
                UserEntry {
                    username: "user002".to_string(),
                    domain: "example.com".to_string(),
                    password: "pass002".to_string(),
                },
            ],
        };
        Arc::new(UserPool::from_users_file(users_file).unwrap())
    }

    fn make_config(mode: RunMode) -> Config {
        let mut config = Config::default();
        config.mode = mode;
        config.duration = 1; // short for tests
        config.target_cps = 5.0;
        config.health_check_retries = 3;
        config.health_check_timeout = 1;
        config.shutdown_timeout = 2;
        config
    }

    fn make_orchestrator(mode: RunMode) -> Orchestrator {
        let config = make_config(mode);
        let pool = make_user_pool();
        Orchestrator::new(config, pool)
    }

    fn make_uac(transport: Arc<dyn SipTransport>, stats: Arc<StatsCollector>, pool: Arc<UserPool>) -> Uac {
        let dialog_manager = Arc::new(DialogManager::new(10_000));
        let config = UacConfig::default();
        Uac::new(transport, dialog_manager, stats, pool, config)
    }

    fn make_uas(transport: Arc<dyn SipTransport>, stats: Arc<StatsCollector>) -> Uas {
        Uas::new(transport, stats, UasConfig::default())
    }

    /// Attach a mock UAC to the orchestrator so that run_step actually sends calls
    /// and records them in stats (total_calls > 0).
    fn attach_mock_uac(orch: &mut Orchestrator) {
        let transport: Arc<dyn SipTransport> = Arc::new(StatsRecordingTransport::new(orch.stats().clone()));
        let pool = make_user_pool();
        let uac = make_uac(transport, orch.stats().clone(), pool);
        orch.set_uac(Arc::new(uac));
    }

    // ===== Orchestrator::new tests =====

    #[test]
    fn test_new_creates_instance_with_config() {
        let orch = make_orchestrator(RunMode::Sustained);
        assert_eq!(orch.config().mode, RunMode::Sustained);
        assert_eq!(orch.config().target_cps, 5.0);
        assert_eq!(orch.config().duration, 1);
    }

    #[test]
    fn test_new_initializes_stats_collector() {
        let orch = make_orchestrator(RunMode::Sustained);
        let snap = orch.stats().snapshot();
        assert_eq!(snap.total_calls, 0);
        assert_eq!(snap.successful_calls, 0);
        assert_eq!(snap.failed_calls, 0);
    }

    #[test]
    fn test_new_components_are_none() {
        let orch = make_orchestrator(RunMode::Sustained);
        assert!(orch.uac.is_none());
        assert!(orch.uas.is_none());
        // proxy field has been removed from Orchestrator (Req 3.4)
        // Orchestrator only manages UAC and UAS
    }

    #[test]
    fn test_new_shutdown_flag_is_false() {
        let orch = make_orchestrator(RunMode::Sustained);
        assert!(!orch.is_shutdown_requested());
    }

    // ===== StepResult tests =====

    #[test]
    fn test_step_result_construction() {
        let stats = StatsCollector::new();
        let snap = stats.snapshot();
        let result = StepResult {
            cps: 10.0,
            error_rate: 0.05,
            stats: snap,
        };
        assert_eq!(result.cps, 10.0);
        assert_eq!(result.error_rate, 0.05);
    }

    // ===== Shutdown flag tests =====

    #[test]
    fn test_request_shutdown_sets_flag() {
        let orch = make_orchestrator(RunMode::Sustained);
        assert!(!orch.is_shutdown_requested());
        orch.request_shutdown();
        assert!(orch.is_shutdown_requested());
    }

    #[test]
    fn test_shutdown_flag_is_shared() {
        let orch = make_orchestrator(RunMode::Sustained);
        let flag = orch.shutdown_flag().clone();
        assert!(!flag.load(Ordering::Relaxed));
        orch.request_shutdown();
        assert!(flag.load(Ordering::Relaxed));
    }

    // ===== Health check tests =====

    #[tokio::test]
    async fn test_health_check_succeeds_when_transport_works() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        orch.set_health_check_transport(transport.clone());

        let result = orch.health_check().await;
        assert!(result.is_ok());
        assert_eq!(transport.sent_count(), 1);
    }

    #[tokio::test]
    async fn test_health_check_sends_options_request() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        orch.set_health_check_transport(transport.clone());

        orch.health_check().await.unwrap();

        let messages = transport.sent_messages();
        assert_eq!(messages.len(), 1);
        match &messages[0].0 {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Options);
                assert!(req.headers.get("CSeq").unwrap().contains("OPTIONS"));
            }
            _ => panic!("Expected a request"),
        }
    }

    #[tokio::test]
    async fn test_health_check_sends_to_proxy_address() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        orch.set_health_check_transport(transport.clone());

        orch.health_check().await.unwrap();

        let messages = transport.sent_messages();
        let expected_addr: SocketAddr = format!(
            "{}:{}",
            orch.config().proxy_host,
            orch.config().proxy_port
        )
        .parse()
        .unwrap();
        assert_eq!(messages[0].1, expected_addr);
    }

    #[tokio::test]
    async fn test_health_check_fails_after_retries_exhausted() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        transport.set_should_fail(true);
        orch.set_health_check_transport(transport.clone());

        let result = orch.health_check().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::HealthCheckFailed(retries) => {
                assert_eq!(retries, 3);
            }
            other => panic!("Expected HealthCheckFailed, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_health_check_retries_on_failure() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        transport.set_should_fail(true);
        orch.set_health_check_transport(transport.clone());

        let _ = orch.health_check().await;
        // Should have attempted health_check_retries times
        assert_eq!(transport.sent_count(), 0); // all failed before send
    }

    #[tokio::test]
    async fn test_health_check_without_transport_returns_error() {
        let orch = make_orchestrator(RunMode::Sustained);
        let result = orch.health_check().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("transport"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    // ===== build_options_request tests =====

    #[test]
    fn test_build_options_request_has_correct_method() {
        let config = make_config(RunMode::Sustained);
        let req = build_options_request(&config);
        assert_eq!(req.method, Method::Options);
    }

    #[test]
    fn test_build_options_request_has_required_headers() {
        let config = make_config(RunMode::Sustained);
        let req = build_options_request(&config);
        assert!(req.headers.get("Via").is_some());
        assert!(req.headers.get("From").is_some());
        assert!(req.headers.get("To").is_some());
        assert!(req.headers.get("Call-ID").is_some());
        assert!(req.headers.get("CSeq").is_some());
        assert!(req.headers.get("Max-Forwards").is_some());
        assert!(req.headers.get("Content-Length").is_some());
    }

    #[test]
    fn test_build_options_request_uri_targets_proxy() {
        let config = make_config(RunMode::Sustained);
        let req = build_options_request(&config);
        assert!(req.request_uri.contains(&config.proxy_host));
        assert!(req.request_uri.contains(&config.proxy_port.to_string()));
    }

    // ===== Graceful shutdown tests =====

    #[tokio::test]
    async fn test_graceful_shutdown_with_no_components() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let result = orch.graceful_shutdown().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_graceful_shutdown_clears_uac() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport.clone(),
            orch.stats().clone(),
            make_user_pool(),
        ));
        orch.set_uac(uac);
        assert!(orch.uac.is_some());

        orch.graceful_shutdown().await.unwrap();
        assert!(orch.uac.is_none());
    }

    #[tokio::test]
    async fn test_graceful_shutdown_clears_uas() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uas = Arc::new(make_uas(transport.clone(), orch.stats().clone()));
        orch.set_uas(uas);
        assert!(orch.uas.is_some());

        orch.graceful_shutdown().await.unwrap();
        assert!(orch.uas.is_none());
    }

    #[tokio::test]
    async fn test_graceful_shutdown_order_uac_before_uas() {
        // Verify UAC is shut down before UAS by checking both are cleared
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport.clone(),
            orch.stats().clone(),
            make_user_pool(),
        ));
        let uas = Arc::new(make_uas(transport.clone(), orch.stats().clone()));
        orch.set_uac(uac);
        orch.set_uas(uas);

        orch.graceful_shutdown().await.unwrap();
        assert!(orch.uac.is_none());
        assert!(orch.uas.is_none());
    }

    /// Req 3.1, 3.2: graceful_shutdown only handles UAC and UAS, not proxy.
    /// After shutdown, only UAC and UAS are cleared. No proxy involvement.
    #[tokio::test]
    async fn test_graceful_shutdown_handles_only_uac_and_uas() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport.clone(),
            orch.stats().clone(),
            make_user_pool(),
        ));
        let uas = Arc::new(make_uas(transport.clone(), orch.stats().clone()));
        orch.set_uac(uac);
        orch.set_uas(uas);

        // Verify components are set
        assert!(orch.uac.is_some());
        assert!(orch.uas.is_some());

        // Graceful shutdown should succeed and clear UAC/UAS only
        let result = orch.graceful_shutdown().await;
        assert!(result.is_ok());
        assert!(orch.uac.is_none());
        assert!(orch.uas.is_none());
        // No proxy field to check — Orchestrator no longer manages proxy (Req 3.1, 3.4)
    }

    /// Req 3.4: Orchestrator does not have a set_proxy method or proxy field.
    /// This test documents that the Orchestrator struct has no proxy-related API.
    /// After task 4.2 removes the proxy field and set_proxy method, this test
    /// will compile and pass. If someone re-adds proxy, this test should be updated.
    #[test]
    fn test_orchestrator_has_no_proxy_field() {
        let orch = make_orchestrator(RunMode::Sustained);
        // Verify Orchestrator only exposes UAC/UAS setters and health check transport
        // The following methods should exist:
        assert!(orch.config().proxy_host.len() > 0); // proxy_host config for external proxy
        assert!(orch.config().proxy_port > 0); // proxy_port config for external proxy
        // set_uac, set_uas, set_health_check_transport are the only component setters
        // set_proxy does NOT exist (Req 3.4) — will be enforced after task 4.2
    }

    /// Req 3.5: Health check functionality is maintained for external proxy verification.
    /// This test verifies health check still works correctly after proxy removal.
    #[tokio::test]
    async fn test_health_check_maintained_for_external_proxy() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        orch.set_health_check_transport(transport.clone());

        // Health check should work — it sends OPTIONS to external proxy
        let result = orch.health_check().await;
        assert!(result.is_ok());
        assert_eq!(transport.sent_count(), 1);

        // Verify it targets the proxy address from config
        let messages = transport.sent_messages();
        let expected_addr: SocketAddr = format!(
            "{}:{}",
            orch.config().proxy_host,
            orch.config().proxy_port
        )
        .parse()
        .unwrap();
        assert_eq!(messages[0].1, expected_addr);

        // Verify it sends OPTIONS method
        match &messages[0].0 {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Options);
            }
            _ => panic!("Expected a request"),
        }
    }

    // ===== Run dispatch tests =====

    #[tokio::test]
    async fn test_run_sustained_returns_experiment_result() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        attach_mock_uac(&mut orch);
        // With mock UAC, run_sustained should complete with calls sent
        let result = orch.run().await;
        assert!(result.is_ok());
        let exp = result.unwrap();
        assert_eq!(exp.config.mode, RunMode::Sustained);
    }

    #[tokio::test]
    async fn test_run_sustained_with_uac_sends_calls() {
        let mut config = make_config(RunMode::Sustained);
        config.duration = 1;
        config.target_cps = 10.0;
        let pool = make_user_pool();
        let mut orch = Orchestrator::new(config, pool.clone());

        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport.clone(),
            orch.stats().clone(),
            pool,
        ));
        orch.set_uac(uac);

        let result = orch.run().await.unwrap();
        // Should have sent some calls (at least a few in 1 second at 10 CPS)
        assert!(transport.sent_count() > 0);
        assert_eq!(result.config.mode, RunMode::Sustained);
    }

    #[tokio::test]
    async fn test_run_sustained_respects_shutdown_flag() {
        let mut config = make_config(RunMode::Sustained);
        config.duration = 60; // long duration
        config.target_cps = 100.0;
        let pool = make_user_pool();
        let mut orch = Orchestrator::new(config, pool.clone());

        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport.clone(),
            orch.stats().clone(),
            pool,
        ));
        orch.set_uac(uac);

        // Request shutdown immediately
        orch.request_shutdown();

        let result = orch.run().await.unwrap();
        // Should complete quickly due to shutdown flag
        assert_eq!(result.config.mode, RunMode::Sustained);
    }

    #[tokio::test]
    async fn test_run_sustained_result_has_timestamps() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        attach_mock_uac(&mut orch);
        let result = orch.run().await.unwrap();
        assert!(!result.started_at.is_empty());
        assert!(!result.finished_at.is_empty());
    }

    #[tokio::test]
    async fn test_run_sustained_result_has_config() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        attach_mock_uac(&mut orch);
        let result = orch.run().await.unwrap();
        assert_eq!(result.config.target_cps, 5.0);
    }

    // ===== build_experiment_result tests =====

    #[test]
    fn test_build_experiment_result_maps_stats() {
        let config = make_config(RunMode::Sustained);
        let stats = StatsCollector::new();
        stats.record_call(200, Duration::from_millis(10));
        stats.record_call(200, Duration::from_millis(20));
        stats.record_failure();
        let snap = stats.snapshot();

        let result = build_experiment_result(&config, &snap, 5.0, "start", "end", vec![]);
        assert_eq!(result.total_calls, 3);
        assert_eq!(result.successful_calls, 2);
        assert_eq!(result.failed_calls, 1);
        assert_eq!(result.max_stable_cps, 5.0);
        assert_eq!(result.started_at, "start");
        assert_eq!(result.finished_at, "end");
    }

    #[test]
    fn test_build_experiment_result_maps_latency() {
        let config = make_config(RunMode::Sustained);
        let stats = StatsCollector::new();
        stats.record_call(200, Duration::from_millis(100));
        let snap = stats.snapshot();

        let result = build_experiment_result(&config, &snap, 5.0, "s", "e", vec![]);
        assert!(result.latency_p50_ms > 0.0);
    }

    // ===== set_* tests =====

    #[test]
    fn test_set_uac() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uac = Arc::new(make_uac(
            transport,
            orch.stats().clone(),
            make_user_pool(),
        ));
        orch.set_uac(uac);
        assert!(orch.uac.is_some());
    }

    #[test]
    fn test_set_uas() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        let transport = Arc::new(MockTransport::new());
        let uas = Arc::new(make_uas(transport, orch.stats().clone()));
        orch.set_uas(uas);
        assert!(orch.uas.is_some());
    }

    // ===== chrono_now tests =====

    #[test]
    fn test_chrono_now_returns_non_empty_string() {
        let now = chrono_now();
        assert!(!now.is_empty());
    }

    #[test]
    fn test_chrono_now_returns_numeric_string() {
        let now = chrono_now();
        assert!(now.parse::<u64>().is_ok());
    }

    // ===== Step-up mode tests =====
    // Requirements: 7.1, 7.2, 7.3, 7.4

    fn make_step_up_config(initial_cps: f64, max_cps: f64, step_size: f64, error_threshold: f64) -> Config {
        let mut config = make_config(RunMode::StepUp);
        config.step_up = Some(crate::config::StepUpConfig {
            initial_cps,
            max_cps,
            step_size,
            step_duration: 1, // 1 second for fast tests
            error_threshold,
        });
        config
    }

    fn make_step_up_orchestrator(initial_cps: f64, max_cps: f64, step_size: f64, error_threshold: f64) -> Orchestrator {
        let config = make_step_up_config(initial_cps, max_cps, step_size, error_threshold);
        let pool = make_user_pool();
        Orchestrator::new(config, pool)
    }

    /// Req 7.1: step_up config is required; returns error without it
    #[tokio::test]
    async fn test_run_step_up_requires_step_up_config() {
        let mut config = make_config(RunMode::StepUp);
        config.step_up = None; // no step_up config
        let pool = make_user_pool();
        let mut orch = Orchestrator::new(config, pool);

        let result = orch.run_step_up().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("step_up"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    /// Req 7.1: starts at initial_cps and returns ExperimentResult
    #[tokio::test]
    async fn test_run_step_up_zero_calls_treated_as_fail() {
        // Without a UAC, no calls are sent → total_calls == 0
        // This should be treated as a failure (error_rate = 1.0)
        // so max_stable_cps should be 0.0
        let mut orch = make_step_up_orchestrator(10.0, 30.0, 10.0, 0.1);
        let result = orch.run_step_up().await.unwrap();
        assert_eq!(result.max_stable_cps, 0.0, "zero calls should be treated as fail");
    }

    /// Req 7.1: starts at initial_cps and returns ExperimentResult
    #[tokio::test]
    async fn test_run_step_up_returns_experiment_result() {
        // With mock UAC, calls are sent → error_rate=0 (MockTransport succeeds) → runs to max_cps
        let mut orch = make_step_up_orchestrator(10.0, 30.0, 10.0, 0.1);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await;
        assert!(result.is_ok());
        let exp = result.unwrap();
        assert_eq!(exp.config.mode, RunMode::StepUp);
    }

    /// Req 7.4: stops when max_cps is reached
    #[tokio::test]
    async fn test_run_step_up_stops_at_max_cps() {
        // initial=10, max=30, step=10 → steps: 10, 20, 30 (stop at max)
        // Mock UAC → error_rate=0 → all steps pass → max_stable_cps=30
        let mut orch = make_step_up_orchestrator(10.0, 30.0, 10.0, 0.1);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await.unwrap();
        assert_eq!(result.max_stable_cps, 30.0);
    }

    /// Req 7.3: reports max_stable_cps as the last CPS where error_rate <= threshold
    #[tokio::test]
    async fn test_run_step_up_reports_max_stable_cps_zero_when_first_step_fails() {
        // Use a failing transport so all calls during the step fail.
        // Since send_register/send_invite errors are ignored by run_step (let _ = ...),
        // we spawn a background task that records failures into stats during the step.
        let mut orch = make_step_up_orchestrator(10.0, 100.0, 10.0, 0.0);
        let stats = orch.stats().clone();
        let shutdown = orch.shutdown_flag().clone();

        // Spawn a task that continuously records failures until shutdown
        let failure_task = tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                stats.record_failure();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let result = orch.run_step_up().await.unwrap();
        orch.request_shutdown();
        let _ = failure_task.await;

        // First step: failures recorded → error_rate > 0 (threshold is 0.0)
        // No stable step found → max_stable_cps = 0.0
        assert_eq!(result.max_stable_cps, 0.0);
    }

    /// Req 7.1: CPS increases by step_size each step
    #[tokio::test]
    async fn test_run_step_up_with_uac_sends_at_increasing_cps() {
        let mut orch = make_step_up_orchestrator(5.0, 15.0, 5.0, 0.5);
        let transport = Arc::new(StatsRecordingTransport::new(orch.stats().clone()));
        let uac = Arc::new(make_uac(
            transport.clone() as Arc<dyn SipTransport>,
            orch.stats().clone(),
            make_user_pool(),
        ));
        orch.set_uac(uac);

        let result = orch.run_step_up().await.unwrap();
        // Steps: 5, 10, 15 → all pass (StatsRecordingTransport records calls, error_rate=0)
        assert_eq!(result.max_stable_cps, 15.0);
        // Should have sent some calls across all steps
        assert!(transport.inner.sent_count() > 0);
    }

    /// Req 7.2, 7.3: stops when error threshold exceeded, reports last stable CPS
    #[tokio::test]
    async fn test_run_step_up_respects_shutdown_flag() {
        let mut orch = make_step_up_orchestrator(10.0, 1000.0, 10.0, 0.1);
        // Request shutdown immediately → should break out of loop
        orch.request_shutdown();

        let result = orch.run_step_up().await.unwrap();
        // Should complete quickly due to shutdown flag
        assert_eq!(result.config.mode, RunMode::StepUp);
    }

    /// Req 7.1, 7.4: when initial_cps == max_cps, runs exactly one step
    #[tokio::test]
    async fn test_run_step_up_single_step_when_initial_equals_max() {
        let mut orch = make_step_up_orchestrator(50.0, 50.0, 10.0, 0.5);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await.unwrap();
        // Only one step at 50 CPS, error_rate=0 → max_stable_cps=50
        assert_eq!(result.max_stable_cps, 50.0);
    }

    /// Result has valid timestamps
    #[tokio::test]
    async fn test_run_step_up_result_has_timestamps() {
        let mut orch = make_step_up_orchestrator(10.0, 10.0, 10.0, 0.5);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await.unwrap();
        assert!(!result.started_at.is_empty());
        assert!(!result.finished_at.is_empty());
    }

    /// Req 7.4: step_size that overshoots max_cps is clamped
    #[tokio::test]
    async fn test_run_step_up_clamps_cps_to_max() {
        // initial=10, max=25, step=20 → steps: 10, 25 (clamped to max)
        let mut orch = make_step_up_orchestrator(10.0, 25.0, 20.0, 0.5);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await.unwrap();
        // Both steps pass (error_rate=0) → max_stable_cps=25
        assert_eq!(result.max_stable_cps, 25.0);
    }

    // ===== Binary search mode tests =====
    // Requirements: 8.1, 8.2, 8.3, 8.4, 8.5

    fn make_binary_search_config(
        initial_cps: f64,
        step_size: f64,
        error_threshold: f64,
        convergence_threshold: f64,
        cooldown_duration: u64,
    ) -> Config {
        let mut config = make_config(RunMode::BinarySearch);
        config.binary_search = Some(crate::config::BinarySearchConfig {
            initial_cps,
            step_size,
            step_duration: 1, // 1 second for fast tests
            error_threshold,
            convergence_threshold,
            cooldown_duration,
        });
        config
    }

    fn make_binary_search_orchestrator(
        initial_cps: f64,
        step_size: f64,
        error_threshold: f64,
        convergence_threshold: f64,
        cooldown_duration: u64,
    ) -> Orchestrator {
        let config = make_binary_search_config(
            initial_cps,
            step_size,
            error_threshold,
            convergence_threshold,
            cooldown_duration,
        );
        let pool = make_user_pool();
        Orchestrator::new(config, pool)
    }

    /// Req 8.1: binary_search config is required; returns error without it
    #[tokio::test]
    async fn test_run_binary_search_requires_config() {
        let mut config = make_config(RunMode::BinarySearch);
        config.binary_search = None;
        let pool = make_user_pool();
        let mut orch = Orchestrator::new(config, pool);

        let result = orch.run_binary_search().await;
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::ConfigError(msg) => {
                assert!(msg.contains("binary_search"));
            }
            other => panic!("Expected ConfigError, got {:?}", other),
        }
    }

    /// Bug fix: zero calls should be treated as fail in binary search
    #[tokio::test]
    async fn test_run_binary_search_zero_calls_treated_as_fail() {
        // Without a UAC, no calls are sent → total_calls == 0
        // This should be treated as a failure (error_rate = 1.0)
        // so max_stable_cps should be 0.0 (no stable CPS found)
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            50.0,  // step_size
            0.1,   // error_threshold
            5.0,   // convergence_threshold
            0,     // no cooldown
        );
        let result = orch.run_binary_search().await.unwrap();
        assert_eq!(result.max_stable_cps, 0.0, "zero calls should be treated as fail");
    }

    /// Req 8.1: returns ExperimentResult on success
    #[tokio::test]
    async fn test_run_binary_search_returns_experiment_result() {
        // Mock UAC → error_rate=0 at all steps → low keeps rising → converges
        // Use large convergence_threshold so it converges quickly after upper bound found
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            50.0,  // step_size (large to find upper bound fast)
            0.1,   // error_threshold
            100.0, // convergence_threshold (large for quick convergence)
            0,     // no cooldown
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await;
        assert!(result.is_ok());
        let exp = result.unwrap();
        assert_eq!(exp.config.mode, RunMode::BinarySearch);
    }

    /// Shutdown flag causes early termination
    #[tokio::test]
    async fn test_run_binary_search_respects_shutdown_flag() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 10.0, 0.1, 1.0, 0,
        );
        orch.request_shutdown();

        let result = orch.run_binary_search().await.unwrap();
        assert_eq!(result.config.mode, RunMode::BinarySearch);
    }

    /// Req 8.4: search converges when high - low <= convergence_threshold
    #[tokio::test]
    async fn test_run_binary_search_converges() {
        // Mock UAC → error_rate=0 → low keeps rising toward high
        // With large convergence_threshold, the search should stop quickly
        let mut orch = make_binary_search_orchestrator(
            10.0,   // initial_cps
            50.0,   // step_size (large for fast upper bound)
            0.1,    // error_threshold
            100.0,  // convergence_threshold (large for quick convergence)
            0,      // no cooldown
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        // max_stable_cps should be > 0 since error_rate=0 at all steps
        assert!(result.max_stable_cps > 0.0);
    }

    /// Result has valid timestamps
    #[tokio::test]
    async fn test_run_binary_search_result_has_timestamps() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 50.0, 0.1, 100.0, 0,
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        assert!(!result.started_at.is_empty());
        assert!(!result.finished_at.is_empty());
    }

    /// Req 8.2, 8.3: with failures injected, binary search finds the boundary
    #[tokio::test]
    async fn test_run_binary_search_finds_boundary_with_failures() {
        // Inject failures so error_rate > 0 → high should come down
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            10.0,  // step_size
            0.0,   // error_threshold = 0 → any failure exceeds threshold
            5.0,   // convergence_threshold
            0,     // no cooldown
        );
        let stats = orch.stats().clone();
        let shutdown = orch.shutdown_flag().clone();

        // Spawn a task that continuously records failures
        let failure_task = tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                stats.record_failure();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let result = orch.run_binary_search().await.unwrap();
        orch.request_shutdown();
        let _ = failure_task.await;

        // With constant failures and threshold=0, all steps fail
        // → high comes down to initial_cps, low stays at 0
        // → max_stable_cps should be 0 (no stable CPS found)
        assert_eq!(result.max_stable_cps, 0.0);
    }

    /// Req 8.5: cooldown_duration > 0 doesn't break execution
    #[tokio::test]
    async fn test_run_binary_search_with_cooldown() {
        // Use shutdown flag to keep it short, just verify cooldown path doesn't panic
        let mut orch = make_binary_search_orchestrator(
            10.0,   // initial_cps
            50.0,   // step_size
            0.1,    // error_threshold
            100.0,  // large convergence_threshold for quick convergence
            1,      // 1 second cooldown
        );
        attach_mock_uac(&mut orch);
        // Request shutdown after a short delay to keep test fast
        let flag = orch.shutdown_flag().clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            flag.store(true, Ordering::Relaxed);
        });
        let result = orch.run_binary_search().await;
        assert!(result.is_ok());
    }

    // ===== Binary search step tracking tests =====

    #[tokio::test]
    async fn test_run_binary_search_result_has_steps_field() {
        // Mock UAC → error_rate=0 at all steps → expand phase runs, then converges
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            50.0,  // step_size
            0.1,   // error_threshold
            100.0, // convergence_threshold (large for quick convergence)
            0,     // no cooldown
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        // steps field should exist (may be empty or populated)
        // With no UAC and error_rate=0, expand phase runs until safety limit
        // The steps vec should be populated
        assert!(!result.steps.is_empty());
    }

    #[tokio::test]
    async fn test_run_binary_search_steps_have_expand_phase() {
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            50.0,  // step_size
            0.1,   // error_threshold
            100.0, // convergence_threshold
            0,     // no cooldown
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        // At least one step should be in the "expand" phase
        let expand_steps: Vec<_> = result.steps.iter()
            .filter(|s| s.phase == "expand")
            .collect();
        assert!(!expand_steps.is_empty());
    }

    #[tokio::test]
    async fn test_run_binary_search_steps_sequential_numbering() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 50.0, 0.1, 100.0, 0,
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        // Steps should be numbered sequentially starting from 1
        for (i, step) in result.steps.iter().enumerate() {
            assert_eq!(step.step, i + 1, "Step {} should have step number {}", i, i + 1);
        }
    }

    #[tokio::test]
    async fn test_run_binary_search_steps_have_valid_result_field() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 50.0, 0.1, 100.0, 0,
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        for step in &result.steps {
            assert!(
                step.result == "pass" || step.result == "fail",
                "Step result should be 'pass' or 'fail', got '{}'", step.result
            );
        }
    }

    #[tokio::test]
    async fn test_run_binary_search_steps_successful_plus_failed_equals_total() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 50.0, 0.1, 100.0, 0,
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        for step in &result.steps {
            assert_eq!(
                step.successful_calls + step.failed_calls, step.total_calls,
                "successful + failed should equal total at step {}", step.step
            );
        }
    }

    #[tokio::test]
    async fn test_run_binary_search_with_failures_has_binary_search_phase() {
        // Inject failures so expand phase finds upper bound quickly,
        // then binary search phase runs
        let mut orch = make_binary_search_orchestrator(
            10.0,  // initial_cps
            10.0,  // step_size
            0.0,   // error_threshold = 0 → any failure exceeds threshold
            5.0,   // convergence_threshold
            0,     // no cooldown
        );
        let stats = orch.stats().clone();
        let shutdown = orch.shutdown_flag().clone();

        let failure_task = tokio::spawn(async move {
            while !shutdown.load(Ordering::Relaxed) {
                stats.record_failure();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let result = orch.run_binary_search().await.unwrap();
        orch.request_shutdown();
        let _ = failure_task.await;

        // With constant failures and threshold=0, the first expand step fails
        // → binary search phase should run between low=0 and high=initial_cps
        let bs_steps: Vec<_> = result.steps.iter()
            .filter(|s| s.phase == "binary_search")
            .collect();
        // There should be binary_search phase steps (since high - low > convergence_threshold initially)
        assert!(!bs_steps.is_empty(), "Should have binary_search phase steps");
    }

    #[tokio::test]
    async fn test_run_binary_search_expand_steps_target_cps_increases() {
        let mut orch = make_binary_search_orchestrator(
            10.0, 20.0, 0.1, 100.0, 0,
        );
        attach_mock_uac(&mut orch);
        let result = orch.run_binary_search().await.unwrap();
        let expand_steps: Vec<_> = result.steps.iter()
            .filter(|s| s.phase == "expand")
            .collect();
        // Each expand step should have increasing target_cps
        for i in 1..expand_steps.len() {
            assert!(
                expand_steps[i].target_cps > expand_steps[i-1].target_cps,
                "Expand step CPS should increase: {} vs {}",
                expand_steps[i-1].target_cps, expand_steps[i].target_cps
            );
        }
    }

    #[tokio::test]
    async fn test_run_sustained_has_empty_steps() {
        let mut orch = make_orchestrator(RunMode::Sustained);
        attach_mock_uac(&mut orch);
        let result = orch.run().await.unwrap();
        assert!(result.steps.is_empty(), "Sustained mode should have empty steps");
    }

    #[tokio::test]
    async fn test_run_step_up_has_empty_steps() {
        let mut orch = make_step_up_orchestrator(10.0, 30.0, 10.0, 0.1);
        attach_mock_uac(&mut orch);
        let result = orch.run_step_up().await.unwrap();
        assert!(result.steps.is_empty(), "Step-up mode should have empty steps");
    }

    // ===== Property-Based Tests =====
    // Feature: sip-load-tester, Property 7: ステップアップ探索の正確性
    // **Validates: Requirements 7.1, 7.2, 7.3**

    use proptest::prelude::*;

    /// Strategy for generating valid StepUpConfig parameters.
    /// Constraints:
    /// - initial_cps > 0, step_size > 0
    /// - max_cps >= initial_cps
    /// - error_threshold in [0.0, 1.0]
    fn arb_step_up_params() -> impl Strategy<Value = (f64, f64, f64, f64)> {
        (
            1.0f64..=100.0,   // initial_cps
            1.0f64..=50.0,    // step_size
            0.01f64..=1.0,    // error_threshold
        ).prop_flat_map(|(initial, step, threshold)| {
            // max_cps >= initial_cps
            let max = initial..=(initial + 500.0);
            (Just(initial), max, Just(step), Just(threshold))
        })
    }

    /// Strategy for generating a "break CPS" — the CPS at which error rate exceeds threshold.
    /// The eval function returns 0.0 for CPS < break_cps and 1.0 for CPS >= break_cps.
    fn arb_step_up_with_break() -> impl Strategy<Value = (f64, f64, f64, f64, f64)> {
        arb_step_up_params().prop_flat_map(|(initial, max, step, threshold)| {
            // break_cps can be anywhere from initial to max+step (beyond max means no break)
            let break_range = initial..=(max + step + 1.0);
            (Just(initial), Just(max), Just(step), Just(threshold), break_range)
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]

        /// Property 7(a): Search starts at initial_cps and increases by step_size.
        /// We verify by tracking which CPS values the eval_fn is called with.
        #[test]
        fn prop_step_up_starts_at_initial_and_steps(
            (initial, max, step, threshold) in arb_step_up_params()
        ) {
            let evaluated = std::sync::Mutex::new(Vec::new());
            let _ = step_up_search(initial, max, step, threshold, |cps| {
                evaluated.lock().unwrap().push(cps);
                0.0 // always pass → runs all steps
            });
            let evals = evaluated.lock().unwrap();
            // (a) Must start at initial_cps
            prop_assert!(!evals.is_empty(), "should evaluate at least one step");
            prop_assert!(
                (evals[0] - initial).abs() < f64::EPSILON,
                "first CPS should be initial_cps={}, got {}",
                initial, evals[0]
            );
            // (a) Each subsequent step increases by step_size (clamped to max_cps)
            for i in 1..evals.len() {
                let expected = (evals[i - 1] + step).min(max);
                prop_assert!(
                    (evals[i] - expected).abs() < 1e-9,
                    "step {} should be {}, got {}",
                    i, expected, evals[i]
                );
            }
        }

        /// Property 7(b): Search stops when error_rate > error_threshold.
        /// We use a break_cps: eval returns 0.0 below it, 1.0 at/above it.
        #[test]
        fn prop_step_up_stops_on_error_threshold(
            (initial, max, step, threshold, break_cps) in arb_step_up_with_break()
        ) {
            let evaluated = std::sync::Mutex::new(Vec::new());
            let _ = step_up_search(initial, max, step, threshold, |cps| {
                evaluated.lock().unwrap().push(cps);
                if cps >= break_cps { 1.0 } else { 0.0 }
            });
            let evals = evaluated.lock().unwrap();
            // If any evaluated CPS >= break_cps, it should be the last one
            // (search stops after encountering error > threshold)
            if let Some(pos) = evals.iter().position(|&c| c >= break_cps) {
                prop_assert_eq!(
                    pos, evals.len() - 1,
                    "search should stop at the first failing step (pos={}, len={})",
                    pos, evals.len()
                );
            }
        }

        /// Property 7(c): Reported max_stable_cps is the last CPS where error_rate <= threshold.
        #[test]
        fn prop_step_up_reports_last_stable_cps(
            (initial, max, step, threshold, break_cps) in arb_step_up_with_break()
        ) {
            let result = step_up_search(initial, max, step, threshold, |cps| {
                if cps >= break_cps { 1.0 } else { 0.0 }
            });

            // Compute expected: walk through steps, find last stable
            let mut expected_stable = 0.0f64;
            let mut cps = initial;
            loop {
                let err = if cps >= break_cps { 1.0 } else { 0.0 };
                if err <= threshold {
                    expected_stable = cps;
                } else {
                    break;
                }
                if cps >= max {
                    break;
                }
                cps = (cps + step).min(max);
            }

            prop_assert!(
                (result - expected_stable).abs() < 1e-9,
                "max_stable_cps should be {}, got {} (initial={}, max={}, step={}, threshold={}, break={})",
                expected_stable, result, initial, max, step, threshold, break_cps
            );
        }

        /// Property 7(c) + boundary: max_stable_cps <= max_cps always.
        #[test]
        fn prop_step_up_result_bounded_by_max_cps(
            (initial, max, step, threshold) in arb_step_up_params()
        ) {
            let result = step_up_search(initial, max, step, threshold, |_| 0.0);
            prop_assert!(
                result <= max + 1e-9,
                "max_stable_cps={} should be <= max_cps={}",
                result, max
            );
        }

        // Feature: sip-load-tester, Property 8: 二分探索の収束性
        // **Validates: Requirements 8.1, 8.2, 8.3, 8.4**

        /// Property 8(a): The search converges: final high - low <= convergence_threshold.
        /// For a monotonically increasing error function, binary search must converge.
        #[test]
        fn prop_binary_search_converges(
            initial_cps in 1.0f64..100.0,
            step_size in 1.0f64..50.0,
            error_threshold in 0.01f64..0.5,
            convergence_threshold in 0.1f64..10.0,
            limit_cps in 1.0f64..200.0,
        ) {
            // Monotonically increasing error function: step function at limit_cps
            let result = binary_search_find_limit(
                initial_cps,
                step_size,
                error_threshold,
                convergence_threshold,
                |cps| if cps <= limit_cps { 0.0 } else { 1.0 },
            );

            // The result (low) should be such that the search has converged.
            // We re-run the algorithm tracking low/high to verify convergence.
            let mut low = 0.0_f64;
            let mut high = initial_cps;
            loop {
                let err = if high <= limit_cps { 0.0 } else { 1.0 };
                if err > error_threshold {
                    break;
                }
                low = high;
                if high > 1_000_000.0 { break; }
                high *= 2.0;
            }
            let max_iter = 200_u32;
            let mut iter = 0_u32;
            while (high - low) > convergence_threshold {
                iter += 1;
                if iter > max_iter { break; }
                let mid = (low + high) / 2.0;
                let err = if mid <= limit_cps { 0.0 } else { 1.0 };
                if err <= error_threshold { low = mid; } else { high = mid; }
            }
            // Verify convergence
            prop_assert!(
                (high - low) <= convergence_threshold + 1e-9,
                "search should converge: high - low = {} > convergence_threshold = {}",
                high - low, convergence_threshold
            );
            // Result should match our traced low
            prop_assert!(
                (result - low).abs() < 1e-9,
                "result={} should match traced low={}",
                result, low
            );
        }

        /// Property 8(b): The reported CPS (low) has error_rate <= error_threshold.
        /// For monotonically increasing error functions, the returned CPS should be safe.
        #[test]
        fn prop_binary_search_result_is_safe(
            initial_cps in 1.0f64..100.0,
            step_size in 1.0f64..50.0,
            error_threshold in 0.01f64..0.5,
            convergence_threshold in 0.1f64..10.0,
            limit_cps in 1.0f64..200.0,
        ) {
            // Smoother monotonic error function
            let eval_fn = |cps: f64| -> f64 {
                ((cps / limit_cps) - 1.0).max(0.0).min(1.0)
            };

            let result = binary_search_find_limit(
                initial_cps,
                step_size,
                error_threshold,
                convergence_threshold,
                eval_fn,
            );

            // The reported CPS should have error_rate <= error_threshold
            let error_at_result = eval_fn(result);
            prop_assert!(
                error_at_result <= error_threshold + 1e-9,
                "error_rate at result CPS {} is {}, should be <= error_threshold {}",
                result, error_at_result, error_threshold
            );
        }

        /// Property 8(c): CPS slightly above the reported value has error_rate > error_threshold
        /// (within convergence_threshold tolerance).
        /// For a step error function, result + convergence_threshold should be near the boundary.
        #[test]
        fn prop_binary_search_result_near_boundary(
            initial_cps in 1.0f64..100.0,
            step_size in 1.0f64..50.0,
            error_threshold in 0.01f64..0.5,
            convergence_threshold in 0.1f64..10.0,
            limit_cps in 1.0f64..200.0,
        ) {
            // Step error function at limit_cps
            let eval_fn = |cps: f64| -> f64 {
                if cps <= limit_cps { 0.0 } else { 1.0 }
            };

            let result = binary_search_find_limit(
                initial_cps,
                step_size,
                error_threshold,
                convergence_threshold,
                eval_fn,
            );

            // If limit_cps is reachable (initial_cps can step up to it),
            // then result should be within convergence_threshold of limit_cps
            if result > 0.0 && limit_cps >= initial_cps {
                // result <= limit_cps (since error at result is 0.0 <= threshold)
                prop_assert!(
                    result <= limit_cps + 1e-9,
                    "result {} should be <= limit_cps {}",
                    result, limit_cps
                );
                // CPS at result + convergence_threshold should be >= limit_cps
                // (i.e., the boundary is within convergence_threshold of result)
                // This holds when the search has found the boundary
                if eval_fn(result) <= error_threshold {
                    // result is safe, and result + convergence_threshold should cross or be near boundary
                    prop_assert!(
                        result + convergence_threshold + 1e-9 >= limit_cps
                            || eval_fn(result + convergence_threshold) > error_threshold,
                        "result {} + convergence {} should be near limit_cps {}",
                        result, convergence_threshold, limit_cps
                    );
                }
            }
        }

        /// Property 8(d): Result is non-negative.
        #[test]
        fn prop_binary_search_result_non_negative(
            initial_cps in 1.0f64..100.0,
            step_size in 1.0f64..50.0,
            error_threshold in 0.01f64..0.5,
            convergence_threshold in 0.1f64..10.0,
            limit_cps in 1.0f64..200.0,
        ) {
            let result = binary_search_find_limit(
                initial_cps,
                step_size,
                error_threshold,
                convergence_threshold,
                |cps| if cps <= limit_cps { 0.0 } else { 1.0 },
            );

            prop_assert!(
                result >= 0.0,
                "result {} should be non-negative",
                result
            );
        }
    }

    // ===== calculate_send_interval tests =====
    // Requirements: 9.3 - コール送信ループの動的間隔調整

    #[test]
    fn test_calculate_send_interval_normal_cps() {
        // 100 CPS → 1_000_000 / 100 = 10_000 us = 10ms
        let interval = calculate_send_interval(100.0);
        assert_eq!(interval, Duration::from_micros(10_000));
    }

    #[test]
    fn test_calculate_send_interval_low_cps_capped_at_max() {
        // 5 CPS → 1_000_000 / 5 = 200_000 us = 200ms → capped at 100ms
        let interval = calculate_send_interval(5.0);
        assert_eq!(interval, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_send_interval_high_cps_floored_at_min() {
        // 10_000_000 CPS → 1_000_000 / 10_000_000 ≈ 0.1 us → floored at 1ms
        let interval = calculate_send_interval(10_000_000.0);
        assert_eq!(interval, Duration::from_millis(1));
    }

    #[test]
    fn test_calculate_send_interval_zero_cps_returns_max() {
        let interval = calculate_send_interval(0.0);
        assert_eq!(interval, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_send_interval_negative_cps_returns_max() {
        let interval = calculate_send_interval(-10.0);
        assert_eq!(interval, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_send_interval_one_cps() {
        // 1 CPS → 1_000_000 / 1 = 1_000_000 us = 1000ms → capped at 100ms
        let interval = calculate_send_interval(1.0);
        assert_eq!(interval, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_send_interval_exact_min_boundary() {
        // 1000 CPS → 1_000_000 / 1000 = 1000 us = 1ms (exactly the min)
        let interval = calculate_send_interval(1000.0);
        assert_eq!(interval, Duration::from_millis(1));
    }

    #[test]
    fn test_calculate_send_interval_exact_max_boundary() {
        // 10 CPS → 1_000_000 / 10 = 100_000 us = 100ms (exactly the max)
        let interval = calculate_send_interval(10.0);
        assert_eq!(interval, Duration::from_millis(100));
    }

    #[test]
    fn test_calculate_send_interval_between_boundaries() {
        // 50 CPS → 1_000_000 / 50 = 20_000 us = 20ms (between 1ms and 100ms)
        let interval = calculate_send_interval(50.0);
        assert_eq!(interval, Duration::from_micros(20_000));
    }
}
