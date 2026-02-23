// Load pattern controller module
//
// Pure computation module that calculates target CPS at any given time
// based on the pattern configuration. Does not actually send calls -
// the orchestrator uses it to pace call generation.
//
// Requirements: 6.1, 6.2, 6.3, 6.4

use std::time::Duration;

use crate::config::PatternConfig;

/// Load pattern controller that calculates target CPS and cumulative calls
/// based on elapsed time and pattern configuration.
pub struct LoadPattern {
    pattern: PatternConfig,
}

impl LoadPattern {
    /// Create a new LoadPattern from a PatternConfig.
    pub fn new(pattern: PatternConfig) -> Self {
        Self { pattern }
    }

    /// Returns the target CPS at the given elapsed time.
    ///
    /// - RampUp: linearly interpolates from 0 to target_cps over duration_secs.
    ///   After the ramp duration, returns target_cps.
    ///   If duration_secs is 0, returns target_cps immediately (like burst).
    /// - Sustained: returns target_cps immediately
    /// - Burst: returns target_cps immediately
    pub fn target_cps_at(&self, elapsed: Duration, target_cps: f64) -> f64 {
        match &self.pattern {
            PatternConfig::RampUp { duration_secs } => {
                let dur = *duration_secs as f64;
                if dur == 0.0 {
                    return target_cps;
                }
                let t = elapsed.as_secs_f64();
                if t >= dur {
                    target_cps
                } else {
                    target_cps * t / dur
                }
            }
            PatternConfig::Sustained { .. } | PatternConfig::Burst => target_cps,
        }
    }

    /// Returns how many new calls should be sent now.
    /// Computes cumulative expected calls at `elapsed` and subtracts `already_sent`.
    /// Never returns a value that would make total exceed the cumulative target.
    pub fn calls_to_send(&self, elapsed: Duration, target_cps: f64, already_sent: u64) -> u64 {
        let expected = self.cumulative_calls_at(elapsed, target_cps) as u64;
        expected.saturating_sub(already_sent)
    }

    /// Returns the cumulative expected calls at the given elapsed time.
    /// This is the integral of target_cps_at from 0 to elapsed.
    ///
    /// - RampUp: integral of linear ramp = 0.5 * t * cps_at_t for t <= duration,
    ///   plus flat portion for t > duration.
    /// - Sustained / Burst: target_cps * elapsed_secs
    pub fn cumulative_calls_at(&self, elapsed: Duration, target_cps: f64) -> f64 {
        let t = elapsed.as_secs_f64();
        match &self.pattern {
            PatternConfig::RampUp { duration_secs } => {
                let dur = *duration_secs as f64;
                if dur == 0.0 {
                    // Instant ramp: same as sustained
                    return target_cps * t;
                }
                if t <= dur {
                    // Area under triangle: 0.5 * t * (target_cps * t / dur)
                    0.5 * target_cps * t * t / dur
                } else {
                    // Triangle area for ramp + rectangle for flat portion
                    let ramp_area = 0.5 * target_cps * dur;
                    let flat_area = target_cps * (t - dur);
                    ramp_area + flat_area
                }
            }
            PatternConfig::Sustained { .. } | PatternConfig::Burst => target_cps * t,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // RampUp pattern tests (Req 6.1)
    // =========================================================================

    #[test]
    fn test_rampup_cps_at_zero_is_zero() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let cps = lp.target_cps_at(Duration::from_secs(0), 100.0);
        assert!((cps - 0.0).abs() < f64::EPSILON, "CPS at t=0 should be 0, got {cps}");
    }

    #[test]
    fn test_rampup_cps_at_half_duration() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let cps = lp.target_cps_at(Duration::from_secs(5), 100.0);
        assert!((cps - 50.0).abs() < f64::EPSILON, "CPS at t=5 should be 50, got {cps}");
    }

    #[test]
    fn test_rampup_cps_at_full_duration() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let cps = lp.target_cps_at(Duration::from_secs(10), 100.0);
        assert!((cps - 100.0).abs() < f64::EPSILON, "CPS at t=10 should be 100, got {cps}");
    }

    #[test]
    fn test_rampup_cps_beyond_duration_stays_at_target() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let cps = lp.target_cps_at(Duration::from_secs(15), 100.0);
        assert!((cps - 100.0).abs() < f64::EPSILON, "CPS beyond ramp should be target, got {cps}");
    }

    #[test]
    fn test_rampup_cps_at_quarter_duration() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 20 });
        let cps = lp.target_cps_at(Duration::from_secs(5), 200.0);
        // 5/20 * 200 = 50
        assert!((cps - 50.0).abs() < f64::EPSILON, "CPS at t=5 of 20s ramp to 200 should be 50, got {cps}");
    }

    #[test]
    fn test_rampup_cps_with_subsecond_precision() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let cps = lp.target_cps_at(Duration::from_millis(2500), 100.0);
        // 2.5/10 * 100 = 25
        assert!((cps - 25.0).abs() < 0.01, "CPS at t=2.5s should be 25, got {cps}");
    }

    // =========================================================================
    // Sustained pattern tests (Req 6.2)
    // =========================================================================

    #[test]
    fn test_sustained_cps_at_zero() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let cps = lp.target_cps_at(Duration::from_secs(0), 100.0);
        assert!((cps - 100.0).abs() < f64::EPSILON, "Sustained CPS at t=0 should be target, got {cps}");
    }

    #[test]
    fn test_sustained_cps_at_midpoint() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let cps = lp.target_cps_at(Duration::from_secs(30), 50.0);
        assert!((cps - 50.0).abs() < f64::EPSILON, "Sustained CPS should be constant, got {cps}");
    }

    #[test]
    fn test_sustained_cps_at_end() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let cps = lp.target_cps_at(Duration::from_secs(60), 75.0);
        assert!((cps - 75.0).abs() < f64::EPSILON, "Sustained CPS at end should be target, got {cps}");
    }

    // =========================================================================
    // Burst pattern tests (Req 6.3)
    // =========================================================================

    #[test]
    fn test_burst_cps_at_zero() {
        let lp = LoadPattern::new(PatternConfig::Burst);
        let cps = lp.target_cps_at(Duration::from_secs(0), 500.0);
        assert!((cps - 500.0).abs() < f64::EPSILON, "Burst CPS at t=0 should be target, got {cps}");
    }

    #[test]
    fn test_burst_cps_at_any_time() {
        let lp = LoadPattern::new(PatternConfig::Burst);
        let cps = lp.target_cps_at(Duration::from_secs(10), 500.0);
        assert!((cps - 500.0).abs() < f64::EPSILON, "Burst CPS should be constant, got {cps}");
    }

    // =========================================================================
    // cumulative_calls_at tests
    // =========================================================================

    #[test]
    fn test_rampup_cumulative_calls_at_zero() {
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let calls = lp.cumulative_calls_at(Duration::from_secs(0), 100.0);
        assert!((calls - 0.0).abs() < f64::EPSILON, "Cumulative calls at t=0 should be 0, got {calls}");
    }

    #[test]
    fn test_rampup_cumulative_calls_at_full_duration() {
        // Integral of linear ramp from 0 to 100 over 10s = 0.5 * 10 * 100 = 500
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let calls = lp.cumulative_calls_at(Duration::from_secs(10), 100.0);
        assert!((calls - 500.0).abs() < 0.01, "Cumulative calls at t=10 should be 500, got {calls}");
    }

    #[test]
    fn test_rampup_cumulative_calls_at_half_duration() {
        // Integral of linear ramp from 0 to 100 over first 5s of 10s ramp
        // CPS at t=5 is 50, area = 0.5 * 5 * 50 = 125
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let calls = lp.cumulative_calls_at(Duration::from_secs(5), 100.0);
        assert!((calls - 125.0).abs() < 0.01, "Cumulative calls at t=5 should be 125, got {calls}");
    }

    #[test]
    fn test_rampup_cumulative_calls_beyond_duration() {
        // Ramp area (0..10) = 500, then flat at 100 CPS for 5 more seconds = 500 + 500 = 1000
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let calls = lp.cumulative_calls_at(Duration::from_secs(15), 100.0);
        assert!((calls - 1000.0).abs() < 0.01, "Cumulative calls at t=15 should be 1000, got {calls}");
    }

    #[test]
    fn test_sustained_cumulative_calls() {
        // 100 CPS for 10 seconds = 1000 calls
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let calls = lp.cumulative_calls_at(Duration::from_secs(10), 100.0);
        assert!((calls - 1000.0).abs() < 0.01, "Sustained cumulative at t=10 should be 1000, got {calls}");
    }

    #[test]
    fn test_burst_cumulative_calls() {
        // 500 CPS for 3 seconds = 1500 calls
        let lp = LoadPattern::new(PatternConfig::Burst);
        let calls = lp.cumulative_calls_at(Duration::from_secs(3), 500.0);
        assert!((calls - 1500.0).abs() < 0.01, "Burst cumulative at t=3 should be 1500, got {calls}");
    }

    // =========================================================================
    // calls_to_send tests
    // =========================================================================

    #[test]
    fn test_calls_to_send_sustained_first_second() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let to_send = lp.calls_to_send(Duration::from_secs(1), 100.0, 0);
        assert_eq!(to_send, 100, "Should send 100 calls in first second at 100 CPS");
    }

    #[test]
    fn test_calls_to_send_sustained_already_sent_some() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let to_send = lp.calls_to_send(Duration::from_secs(1), 100.0, 50);
        assert_eq!(to_send, 50, "Should send 50 more calls when 50 already sent");
    }

    #[test]
    fn test_calls_to_send_sustained_already_sent_all() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let to_send = lp.calls_to_send(Duration::from_secs(1), 100.0, 100);
        assert_eq!(to_send, 0, "Should send 0 calls when all already sent");
    }

    #[test]
    fn test_calls_to_send_rampup_first_second() {
        // Ramp from 0 to 100 over 10s. At t=1, cumulative = 0.5 * 1 * 10 = 5
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 10 });
        let to_send = lp.calls_to_send(Duration::from_secs(1), 100.0, 0);
        assert_eq!(to_send, 5, "Ramp-up should send 5 calls in first second");
    }

    #[test]
    fn test_calls_to_send_never_negative() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        // already_sent exceeds expected (e.g., timing jitter)
        let to_send = lp.calls_to_send(Duration::from_secs(1), 100.0, 200);
        assert_eq!(to_send, 0, "Should never return negative calls to send");
    }

    // =========================================================================
    // Edge cases
    // =========================================================================

    #[test]
    fn test_rampup_zero_duration_returns_target_immediately() {
        // Edge case: duration_secs = 0 means instant ramp (like burst)
        let lp = LoadPattern::new(PatternConfig::RampUp { duration_secs: 0 });
        let cps = lp.target_cps_at(Duration::from_secs(0), 100.0);
        assert!((cps - 100.0).abs() < f64::EPSILON, "Zero-duration ramp should return target CPS, got {cps}");
    }

    #[test]
    fn test_target_cps_zero() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let cps = lp.target_cps_at(Duration::from_secs(5), 0.0);
        assert!((cps - 0.0).abs() < f64::EPSILON, "Zero target CPS should return 0, got {cps}");
    }

    #[test]
    fn test_calls_to_send_zero_target_cps() {
        let lp = LoadPattern::new(PatternConfig::Sustained { duration_secs: 60 });
        let to_send = lp.calls_to_send(Duration::from_secs(5), 0.0, 0);
        assert_eq!(to_send, 0, "Zero target CPS should send 0 calls");
    }
}
