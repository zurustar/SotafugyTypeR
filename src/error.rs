#[derive(Debug, thiserror::Error)]
pub enum SipLoadTestError {
    #[error("SIP parse error: {0}")]
    ParseError(String),
    #[error("Network error: {0}")]
    NetworkError(#[from] std::io::Error),
    #[error("Dialog not found: {0}")]
    DialogNotFound(String),
    #[error("Dialog timeout: {0}")]
    DialogTimeout(String),
    #[error("Authentication failed: {0}")]
    AuthenticationFailed(String),
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("User pool error: {0}")]
    UserPoolError(String),
    #[error("User pool is empty")]
    EmptyUserPool,
    #[error("Health check failed after {0} retries")]
    HealthCheckFailed(u32),
    #[error("Shutdown timeout")]
    ShutdownTimeout,
    #[error("Max dialogs reached: {0}")]
    MaxDialogsReached(usize),
    #[error("Transaction not found: {0}")]
    TransactionNotFound(String),
    #[error("Max transactions reached: {0}")]
    MaxTransactionsReached(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn parse_error_display() {
        let err = SipLoadTestError::ParseError("invalid header".to_string());
        assert_eq!(err.to_string(), "SIP parse error: invalid header");
    }

    #[test]
    fn network_error_display() {
        let io_err = io::Error::new(io::ErrorKind::ConnectionRefused, "connection refused");
        let err = SipLoadTestError::NetworkError(io_err);
        assert_eq!(err.to_string(), "Network error: connection refused");
    }

    #[test]
    fn network_error_from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::AddrInUse, "address in use");
        let err: SipLoadTestError = io_err.into();
        assert!(matches!(err, SipLoadTestError::NetworkError(_)));
        assert_eq!(err.to_string(), "Network error: address in use");
    }

    #[test]
    fn dialog_not_found_display() {
        let err = SipLoadTestError::DialogNotFound("call-123".to_string());
        assert_eq!(err.to_string(), "Dialog not found: call-123");
    }

    #[test]
    fn dialog_timeout_display() {
        let err = SipLoadTestError::DialogTimeout("call-456".to_string());
        assert_eq!(err.to_string(), "Dialog timeout: call-456");
    }

    #[test]
    fn authentication_failed_display() {
        let err = SipLoadTestError::AuthenticationFailed("bad credentials".to_string());
        assert_eq!(err.to_string(), "Authentication failed: bad credentials");
    }

    #[test]
    fn config_error_display() {
        let err = SipLoadTestError::ConfigError("missing field".to_string());
        assert_eq!(err.to_string(), "Configuration error: missing field");
    }

    #[test]
    fn user_pool_error_display() {
        let err = SipLoadTestError::UserPoolError("file not found".to_string());
        assert_eq!(err.to_string(), "User pool error: file not found");
    }

    #[test]
    fn empty_user_pool_display() {
        let err = SipLoadTestError::EmptyUserPool;
        assert_eq!(err.to_string(), "User pool is empty");
    }

    #[test]
    fn health_check_failed_display() {
        let err = SipLoadTestError::HealthCheckFailed(3);
        assert_eq!(err.to_string(), "Health check failed after 3 retries");
    }

    #[test]
    fn shutdown_timeout_display() {
        let err = SipLoadTestError::ShutdownTimeout;
        assert_eq!(err.to_string(), "Shutdown timeout");
    }

    #[test]
    fn transaction_not_found_display() {
        let err = SipLoadTestError::TransactionNotFound("tx-abc123".to_string());
        assert_eq!(err.to_string(), "Transaction not found: tx-abc123");
    }

    #[test]
    fn max_transactions_reached_display() {
        let err = SipLoadTestError::MaxTransactionsReached(1000);
        assert_eq!(err.to_string(), "Max transactions reached: 1000");
    }

    #[test]
    fn transaction_not_found_matches_pattern() {
        let err = SipLoadTestError::TransactionNotFound("branch-001".to_string());
        assert!(matches!(err, SipLoadTestError::TransactionNotFound(ref s) if s == "branch-001"));
    }

    #[test]
    fn max_transactions_reached_matches_pattern() {
        let err = SipLoadTestError::MaxTransactionsReached(500);
        assert!(matches!(err, SipLoadTestError::MaxTransactionsReached(500)));
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SipLoadTestError>();
    }

    #[test]
    fn error_implements_std_error() {
        let err = SipLoadTestError::ParseError("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn error_debug_impl() {
        let err = SipLoadTestError::EmptyUserPool;
        let debug_str = format!("{:?}", err);
        assert!(debug_str.contains("EmptyUserPool"));
    }
}
