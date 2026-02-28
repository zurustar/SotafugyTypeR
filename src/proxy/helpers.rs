// Proxy helper/utility functions
//
// Standalone helper functions used by SipProxy for URI parsing,
// Via header parsing, branch generation, and URI extraction.

use std::net::SocketAddr;

/// Parse a SIP URI from a Route or Record-Route header to extract the socket address.
/// Handles formats like: <sip:host:port;lr>, sip:host:port, etc.
pub(crate) fn parse_uri_addr(uri: &str) -> Option<SocketAddr> {
    let uri = uri.trim();
    // Strip angle brackets
    let uri = if uri.starts_with('<') && uri.contains('>') {
        &uri[1..uri.find('>')?]
    } else {
        uri
    };
    // Strip "sip:" prefix
    let host_port = uri.strip_prefix("sip:")?;
    // Strip parameters (;lr, etc.)
    let host_port = host_port.split(';').next()?;
    host_port.parse().ok()
}

/// Parse a Via header to extract the address for response routing.
/// Via format: SIP/2.0/UDP host:port;branch=xxx
pub(crate) fn parse_via_addr(via: &str) -> Option<SocketAddr> {
    let via = via.trim();
    // Skip "SIP/2.0/UDP " prefix
    let after_proto = via.split_whitespace().nth(1)?;
    // Strip parameters to get base host:port
    let host_port = after_proto.split(';').next()?;
    let base_addr: SocketAddr = host_port.parse().ok()?;

    // Extract received and rport parameters
    let mut received_ip: Option<std::net::IpAddr> = None;
    let mut rport_val: Option<u16> = None;

    for param in after_proto.split(';').skip(1) {
        let param = param.trim();
        if let Some(val) = param.strip_prefix("received=") {
            received_ip = val.parse().ok();
        } else if let Some(val) = param.strip_prefix("rport=") {
            rport_val = val.parse().ok();
        }
    }

    let ip = received_ip.unwrap_or(base_addr.ip());
    let port = rport_val.unwrap_or(base_addr.port());
    Some(SocketAddr::new(ip, port))
}

/// Generate a random branch suffix for Via headers.
pub(crate) fn rand_branch() -> String {
    use rand::Rng;
    let val: u64 = rand::thread_rng().gen();
    format!("{:016x}", val)
}

/// Extract a SIP URI from a header value.
/// Handles formats like: "<sip:alice@example.com>;tag=xxx" -> "sip:alice@example.com"
/// or "sip:alice@example.com" -> "sip:alice@example.com"
pub(crate) fn extract_uri(header_value: &str) -> String {
    let trimmed = header_value.trim();
    if let Some(start) = trimmed.find('<') {
        if let Some(end) = trimmed.find('>') {
            return trimmed[start + 1..end].to_string();
        }
    }
    // No angle brackets: take up to first ';' or space
    trimmed.split(';').next().unwrap_or(trimmed).split_whitespace().next().unwrap_or(trimmed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== parse_uri_addr tests =====

    #[test]
    fn test_parse_uri_addr_with_angle_brackets_and_lr() {
        let addr = parse_uri_addr("<sip:10.0.0.2:5060;lr>");
        assert_eq!(addr, Some("10.0.0.2:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_uri_addr_without_angle_brackets() {
        let addr = parse_uri_addr("sip:10.0.0.2:5060");
        assert_eq!(addr, Some("10.0.0.2:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_uri_addr_invalid() {
        assert!(parse_uri_addr("not-a-uri").is_none());
    }

    // ===== parse_via_addr tests =====

    #[test]
    fn test_parse_via_addr_standard() {
        let addr = parse_via_addr("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776");
        assert_eq!(addr, Some("10.0.0.1:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_invalid() {
        assert!(parse_via_addr("invalid").is_none());
    }

    // ===== parse_via_addr received/rport tests =====

    #[test]
    fn test_parse_via_addr_received_only() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;received=192.168.1.1;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("192.168.1.1:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_rport_with_value() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;rport=5062;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("10.0.0.1:5062".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_received_and_rport() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;received=192.168.1.1;rport=5062;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("192.168.1.1:5062".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_rport_without_value() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;rport;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("10.0.0.1:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_no_received_no_rport() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("10.0.0.1:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_rport_and_received_reversed_order() {
        let addr = parse_via_addr(
            "SIP/2.0/UDP 10.0.0.1:5060;rport=5062;received=192.168.1.1;branch=z9hG4bK776",
        );
        assert_eq!(addr, Some("192.168.1.1:5062".parse().unwrap()));
    }

    // ===== extract_uri helper tests =====

    #[test]
    fn test_extract_uri_with_angle_brackets_and_tag() {
        assert_eq!(
            extract_uri("<sip:alice@example.com>;tag=1928301774"),
            "sip:alice@example.com"
        );
    }

    #[test]
    fn test_extract_uri_with_angle_brackets_only() {
        assert_eq!(
            extract_uri("<sip:bob@example.com>"),
            "sip:bob@example.com"
        );
    }

    #[test]
    fn test_extract_uri_without_angle_brackets() {
        assert_eq!(
            extract_uri("sip:alice@example.com"),
            "sip:alice@example.com"
        );
    }

    // ===== rand_branch tests =====

    #[test]
    fn test_rand_branch_returns_16_char_hex_string() {
        let branch = rand_branch();
        assert_eq!(
            branch.len(),
            16,
            "rand_branch should return 16-char hex string, got: '{}'",
            branch
        );
        assert!(
            branch.chars().all(|c| c.is_ascii_hexdigit()),
            "rand_branch should contain only hex digits, got: '{}'",
            branch
        );
    }

    #[test]
    fn test_rand_branch_produces_unique_values() {
        let mut branches: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..100 {
            let branch = rand_branch();
            branches.insert(branch);
        }
        assert_eq!(
            branches.len(),
            100,
            "100 consecutive rand_branch() calls should produce 100 unique values"
        );
    }
}
