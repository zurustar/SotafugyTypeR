use std::net::SocketAddr;

/// Proxy configuration
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub host: String,
    pub port: u16,
    /// Default forwarding destination (UAS address)
    pub forward_addr: SocketAddr,
    /// Managed domain: Request-URIs targeting this domain use LocationService
    pub domain: String,
    /// Enable debug logging to stderr
    pub debug: bool,
}
