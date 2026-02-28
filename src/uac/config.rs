// UAC configuration types

use std::net::SocketAddr;
use std::time::Duration;

/// UAC configuration
#[derive(Debug, Clone)]
pub struct UacConfig {
    pub proxy_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub call_duration: Duration,
    pub dialog_timeout: Duration,
    pub session_expires: Duration,
}

impl Default for UacConfig {
    fn default() -> Self {
        Self {
            proxy_addr: "127.0.0.1:5060".parse().unwrap(),
            local_addr: "127.0.0.1:5060".parse().unwrap(),
            call_duration: Duration::from_secs(3),
            dialog_timeout: Duration::from_secs(32),
            session_expires: Duration::from_secs(300),
        }
    }
}

/// Result of background REGISTER operation.
#[derive(Debug, Clone, PartialEq)]
pub struct BgRegisterResult {
    pub success: u32,
    pub failed: u32,
}
