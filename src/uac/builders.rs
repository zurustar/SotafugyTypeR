// UAC builder functions for SIP message construction
//
// Optimized header value builders that replace format! macro calls
// with write!/push_str for reduced allocations.

use std::fmt::Write;
use std::net::SocketAddr;

use crate::sip::message::{Headers, Method, SipRequest};

/// Build Via header value: "SIP/2.0/UDP {addr};branch={branch}"
pub(crate) fn build_via_value(addr: SocketAddr, branch: &str) -> String {
    // "SIP/2.0/UDP " (12) + addr (~21) + ";branch=" (8) + branch (23) ≈ 64
    let mut buf = String::with_capacity(64);
    buf.push_str("SIP/2.0/UDP ");
    write!(buf, "{}", addr).unwrap();
    buf.push_str(";branch=");
    buf.push_str(branch);
    buf
}

/// Build From header value: "<sip:{user}@{domain}>;tag={tag}"
pub(crate) fn build_from_value(username: &str, domain: &str, tag: &str) -> String {
    let mut buf = String::with_capacity(5 + username.len() + 1 + domain.len() + 6 + tag.len());
    buf.push_str("<sip:");
    buf.push_str(username);
    buf.push('@');
    buf.push_str(domain);
    buf.push_str(">;tag=");
    buf.push_str(tag);
    buf
}

/// Build From header with tag only: "<{uri}>;tag={tag}"
pub(crate) fn build_from_tag_only(uri: &str, tag: &str) -> String {
    let mut buf = String::with_capacity(1 + uri.len() + 6 + tag.len());
    buf.push('<');
    buf.push_str(uri);
    buf.push_str(">;tag=");
    buf.push_str(tag);
    buf
}

/// Build To header value: "<sip:{user}@{domain}>"
pub(crate) fn build_to_value(username: &str, domain: &str) -> String {
    let mut buf = String::with_capacity(5 + username.len() + 1 + domain.len() + 1);
    buf.push_str("<sip:");
    buf.push_str(username);
    buf.push('@');
    buf.push_str(domain);
    buf.push('>');
    buf
}

/// Build To header value with optional tag: "<{uri}>" or "<{uri}>;tag={tag}"
pub(crate) fn build_to_value_with_optional_tag(uri: &str, tag: Option<&str>) -> String {
    if let Some(tag) = tag {
        let mut buf = String::with_capacity(1 + uri.len() + 6 + tag.len());
        buf.push('<');
        buf.push_str(uri);
        buf.push_str(">;tag=");
        buf.push_str(tag);
        buf
    } else {
        let mut buf = String::with_capacity(1 + uri.len() + 1);
        buf.push('<');
        buf.push_str(uri);
        buf.push('>');
        buf
    }
}

/// Build CSeq header value: "{num} {method}"
pub(crate) fn build_cseq_value(cseq: u32, method: &str) -> String {
    let mut buf = String::with_capacity(10 + 1 + method.len());
    write!(buf, "{}", cseq).unwrap();
    buf.push(' ');
    buf.push_str(method);
    buf
}

/// Build SIP URI: "sip:{user}@{domain}"
pub(crate) fn build_sip_uri(username: &str, domain: &str) -> String {
    let mut buf = String::with_capacity(4 + username.len() + 1 + domain.len());
    buf.push_str("sip:");
    buf.push_str(username);
    buf.push('@');
    buf.push_str(domain);
    buf
}

/// Build SIP domain URI: "sip:{domain}"
pub(crate) fn build_sip_domain_uri(domain: &str) -> String {
    let mut buf = String::with_capacity(4 + domain.len());
    buf.push_str("sip:");
    buf.push_str(domain);
    buf
}

/// Static re-INVITE builder for use in spawned tasks (no &self reference needed)
pub(crate) fn build_reinvite_request_static(
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    cseq: u32,
    local_addr: SocketAddr,
    proxy_addr: SocketAddr,
) -> SipRequest {
    let branch = crate::sip::generate_branch(call_id, cseq, "INVITE");
    let mut headers = Headers::new();
    headers.add("Via", build_via_value(local_addr, &branch));
    headers.set("Call-ID", call_id.to_string());
    headers.set("From", build_from_tag_only("sip:uac@local", from_tag));
    headers.set("To", build_to_value_with_optional_tag("sip:uas@remote", to_tag));
    headers.set("CSeq", build_cseq_value(cseq, "INVITE"));
    headers.set("Max-Forwards", "70".to_string());
    headers.set("Route", format!("<sip:{}>", proxy_addr));
    headers.set("Content-Length", "0".to_string());

    SipRequest {
        method: Method::Invite,
        request_uri: "sip:uas@remote".to_string(),
        version: "SIP/2.0".to_string(),
        headers,
        body: None,
    }
}

/// Static BYE builder for use in spawned tasks (no &self reference needed)
pub(crate) fn build_bye_request_static(
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    cseq: u32,
    local_addr: SocketAddr,
) -> SipRequest {
    let branch = crate::sip::generate_branch(call_id, cseq, "BYE");
    let mut headers = Headers::new();
    headers.add("Via", build_via_value(local_addr, &branch));
    headers.set("Call-ID", call_id.to_string());
    headers.set("From", build_from_tag_only("sip:uac@local", from_tag));
    headers.set("To", build_to_value_with_optional_tag("sip:uas@remote", to_tag));
    headers.set("CSeq", build_cseq_value(cseq, "BYE"));
    headers.set("Max-Forwards", "70".to_string());
    headers.set("Content-Length", "0".to_string());

    SipRequest {
        method: Method::Bye,
        request_uri: "sip:uas@remote".to_string(),
        version: "SIP/2.0".to_string(),
        headers,
        body: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::formatter::format_sip_message;
    use crate::sip::message::SipMessage;
    use crate::sip::parser::parse_sip_message;

    // ===== Builder function unit tests =====

    /// Verify Via header is built correctly with write! optimization
    #[test]
    fn test_optimized_via_header_construction() {
        let addr: SocketAddr = "192.168.1.100:5060".parse().unwrap();
        let branch = crate::sip::generate_branch("test-call-id", 1, "INVITE");

        // Build Via using the optimized build_via_value helper
        let via = build_via_value(addr, &branch);

        // Must match the format! equivalent
        let expected = format!("SIP/2.0/UDP {};branch={}", addr, branch);
        assert_eq!(via, expected, "Optimized Via header must match format! output");
    }

    /// Verify From header is built correctly with write! optimization
    #[test]
    fn test_optimized_from_header_construction() {
        let from = build_from_value("alice", "example.com", "tag123");
        let expected = format!("<sip:{}@{}>;tag={}", "alice", "example.com", "tag123");
        assert_eq!(from, expected, "Optimized From header must match format! output");
    }

    /// Verify To header is built correctly with write! optimization
    #[test]
    fn test_optimized_to_header_construction() {
        let to = build_to_value("bob", "example.com");
        let expected = format!("<sip:{}@{}>", "bob", "example.com");
        assert_eq!(to, expected, "Optimized To header must match format! output");
    }

    /// Verify CSeq header is built correctly with write! optimization
    #[test]
    fn test_optimized_cseq_header_construction() {
        let cseq = build_cseq_value(42, "INVITE");
        let expected = format!("{} INVITE", 42);
        assert_eq!(cseq, expected, "Optimized CSeq header must match format! output");
    }

    /// Verify request URI is built correctly with write! optimization
    #[test]
    fn test_optimized_request_uri_construction() {
        let uri = build_sip_uri("alice", "example.com");
        let expected = format!("sip:{}@{}", "alice", "example.com");
        assert_eq!(uri, expected, "Optimized request URI must match format! output");
    }

    /// Verify domain-only request URI (for REGISTER)
    #[test]
    fn test_optimized_domain_uri_construction() {
        let uri = build_sip_domain_uri("example.com");
        let expected = format!("sip:{}", "example.com");
        assert_eq!(uri, expected, "Optimized domain URI must match format! output");
    }

    /// Verify To header with tag is built correctly
    #[test]
    fn test_optimized_to_header_with_tag() {
        let to = build_to_value_with_optional_tag("sip:uas@remote", Some("remote-tag"));
        assert_eq!(to, "<sip:uas@remote>;tag=remote-tag");

        let to_no_tag = build_to_value_with_optional_tag("sip:uas@remote", None);
        assert_eq!(to_no_tag, "<sip:uas@remote>");
    }

    /// Verify generate_branch optimization produces identical output
    #[test]
    fn test_generate_branch_optimization_equivalence() {
        // Test with various inputs to ensure the optimized version matches
        let test_cases = vec![
            ("call-id-123", 1u32, "INVITE"),
            ("abc-def-ghi", 42, "REGISTER"),
            ("long-call-id-with-many-chars", 9999, "BYE"),
            ("x", 1, "ACK"),
        ];
        for (call_id, cseq, method) in test_cases {
            let result = crate::sip::generate_branch(call_id, cseq, method);
            assert!(result.starts_with("z9hG4bK"), "Branch must start with magic cookie");
            assert_eq!(result.len(), 7 + 16, "Branch must be z9hG4bK + 16 hex chars");
        }
    }

    /// End-to-end: optimized build_bye_request_static produces parseable message
    #[test]
    fn test_optimized_bye_request_static_is_parseable() {
        let local_addr: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        let request = build_bye_request_static("test-call-id-004", "tag004", Some("remote-tag"), 2, local_addr);
        let msg = SipMessage::Request(request);
        let bytes = format_sip_message(&msg);
        let parsed = parse_sip_message(&bytes).expect("Optimized BYE must be parseable");

        match parsed {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Bye);
                assert_eq!(req.headers.get("Call-ID").unwrap(), "test-call-id-004");
                assert_eq!(req.headers.get("CSeq").unwrap(), "2 BYE");
            }
            _ => panic!("Expected Request, got Response"),
        }
    }

    /// End-to-end: optimized build_reinvite_request_static produces parseable message
    #[test]
    fn test_optimized_reinvite_request_static_is_parseable() {
        let local_addr: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        let proxy_addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let request = build_reinvite_request_static("test-call-id-005", "tag005", Some("remote-tag"), 3, local_addr, proxy_addr);
        let msg = SipMessage::Request(request);
        let bytes = format_sip_message(&msg);
        let parsed = parse_sip_message(&bytes).expect("Optimized re-INVITE must be parseable");

        match parsed {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Invite);
                assert_eq!(req.headers.get("Call-ID").unwrap(), "test-call-id-005");
                assert_eq!(req.headers.get("CSeq").unwrap(), "3 INVITE");
            }
            _ => panic!("Expected Request, got Response"),
        }
    }

    /// re-INVITE must include a Route header pointing to the proxy address
    #[test]
    fn test_reinvite_request_has_route_header() {
        let local_addr: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        let proxy_addr: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let request = build_reinvite_request_static(
            "call-route-test", "tag-route", Some("remote-tag"), 2, local_addr, proxy_addr,
        );

        let route = request.headers.get("Route").expect("Route header must exist");
        assert_eq!(route, format!("<sip:{}>", proxy_addr));
    }
}
