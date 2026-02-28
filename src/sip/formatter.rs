// SIP message formatter
// Converts SipMessage structs into RFC 3261 compliant byte sequences

use super::message::{SipMessage};

/// Estimate the output size of a formatted SIP message to pre-allocate buffer.
/// The estimate is intentionally slightly over to avoid re-allocation.
pub fn estimate_message_size(msg: &SipMessage) -> usize {
    match msg {
        SipMessage::Request(req) => {
            // Request line: METHOD SP URI SP VERSION CRLF
            let mut size = req.method.to_string().len() + 1
                + req.request_uri.len() + 1
                + req.version.len() + 2;
            // Headers
            for h in req.headers.entries() {
                // "Name: Value\r\n"
                size += h.name.len() + 2 + h.value.len() + 2;
            }
            // Possible auto-added Content-Length header (up to ~25 bytes)
            size += 32;
            // Empty line separator
            size += 2;
            // Body
            if let Some(body) = &req.body {
                size += body.len();
            }
            size
        }
        SipMessage::Response(resp) => {
            // Status line: VERSION SP STATUS_CODE SP REASON CRLF
            let mut size = resp.version.len() + 1
                + 3 + 1  // status code is always 3 digits for SIP
                + resp.reason_phrase.len() + 2;
            // Headers
            for h in resp.headers.entries() {
                size += h.name.len() + 2 + h.value.len() + 2;
            }
            // Possible auto-added Content-Length header
            size += 32;
            // Empty line separator
            size += 2;
            // Body
            if let Some(body) = &resp.body {
                size += body.len();
            }
            size
        }
    }
}

/// Format a SIP message into an existing buffer (for buffer reuse).
/// The buffer is NOT cleared — caller should clear it if reuse is intended.
pub fn format_into(buf: &mut Vec<u8>, msg: &SipMessage) {
    match msg {
        SipMessage::Request(req) => {
            // Request line
            buf.extend_from_slice(req.method.to_string().as_bytes());
            buf.push(b' ');
            buf.extend_from_slice(req.request_uri.as_bytes());
            buf.push(b' ');
            buf.extend_from_slice(req.version.as_bytes());
            buf.extend_from_slice(b"\r\n");

            // Headers
            for header in req.headers.entries() {
                buf.extend_from_slice(header.name.as_bytes());
                buf.extend_from_slice(b": ");
                buf.extend_from_slice(header.value.as_bytes());
                buf.extend_from_slice(b"\r\n");
            }

            // Body handling
            if let Some(body) = &req.body {
                if !body.is_empty() {
                    // Add Content-Length if not already present
                    if req.headers.get("Content-Length").is_none() {
                        buf.extend_from_slice(b"Content-Length: ");
                        let mut itoa_buf = itoa::Buffer::new();
                        buf.extend_from_slice(itoa_buf.format(body.len()).as_bytes());
                        buf.extend_from_slice(b"\r\n");
                    }
                    buf.extend_from_slice(b"\r\n");
                    buf.extend_from_slice(body);
                } else {
                    buf.extend_from_slice(b"\r\n");
                }
            } else {
                buf.extend_from_slice(b"\r\n");
            }
        }
        SipMessage::Response(resp) => {
            // Status line
            buf.extend_from_slice(resp.version.as_bytes());
            buf.push(b' ');
            let mut itoa_buf = itoa::Buffer::new();
            buf.extend_from_slice(itoa_buf.format(resp.status_code).as_bytes());
            buf.push(b' ');
            buf.extend_from_slice(resp.reason_phrase.as_bytes());
            buf.extend_from_slice(b"\r\n");

            // Headers
            for header in resp.headers.entries() {
                buf.extend_from_slice(header.name.as_bytes());
                buf.extend_from_slice(b": ");
                buf.extend_from_slice(header.value.as_bytes());
                buf.extend_from_slice(b"\r\n");
            }

            // Body handling
            if let Some(body) = &resp.body {
                if !body.is_empty() {
                    // Add Content-Length if not already present
                    if resp.headers.get("Content-Length").is_none() {
                        buf.extend_from_slice(b"Content-Length: ");
                        let mut itoa_buf = itoa::Buffer::new();
                        buf.extend_from_slice(itoa_buf.format(body.len()).as_bytes());
                        buf.extend_from_slice(b"\r\n");
                    }
                    buf.extend_from_slice(b"\r\n");
                    buf.extend_from_slice(body);
                } else {
                    buf.extend_from_slice(b"\r\n");
                }
            } else {
                buf.extend_from_slice(b"\r\n");
            }
        }
    }
}

/// Format a SipMessage into RFC 3261 compliant bytes.
///
/// Request format:
///   METHOD Request-URI SIP/2.0\r\n
///   Header-Name: Header-Value\r\n
///   ...\r\n
///   \r\n
///   [body]
///
/// Response format:
///   SIP/2.0 Status-Code Reason-Phrase\r\n
///   Header-Name: Header-Value\r\n
///   ...\r\n
///   \r\n
///   [body]
pub fn format_sip_message(msg: &SipMessage) -> Vec<u8> {
    let estimated_size = estimate_message_size(msg);
    let mut buf = Vec::with_capacity(estimated_size);
    format_into(&mut buf, msg);
    buf
}

/// MessageBuf の出力バッファを使用して SIP メッセージをフォーマットする。
/// buf.output をクリアしてからフォーマット結果を書き込む。
pub fn format_into_pooled(buf: &mut super::pool::MessageBuf, msg: &SipMessage) {
    buf.output.clear();
    format_into(&mut buf.output, msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method, SipRequest, SipResponse};

    // --- Unit tests: Method Display trait (replaces method_to_str tests) ---

    #[test]
    fn test_method_display_register() {
        assert_eq!(Method::Register.to_string(), "REGISTER");
    }

    #[test]
    fn test_method_display_invite() {
        assert_eq!(Method::Invite.to_string(), "INVITE");
    }

    #[test]
    fn test_method_display_ack() {
        assert_eq!(Method::Ack.to_string(), "ACK");
    }

    #[test]
    fn test_method_display_bye() {
        assert_eq!(Method::Bye.to_string(), "BYE");
    }

    #[test]
    fn test_method_display_options() {
        assert_eq!(Method::Options.to_string(), "OPTIONS");
    }

    #[test]
    fn test_method_display_update() {
        assert_eq!(Method::Update.to_string(), "UPDATE");
    }

    #[test]
    fn test_method_display_other() {
        assert_eq!(
            Method::Other("SUBSCRIBE".to_string()).to_string(),
            "SUBSCRIBE"
        );
    }

    // --- Unit tests: Request formatting ---

    #[test]
    fn test_format_register_request_no_body() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("From", "<sip:alice@example.com>;tag=1234".to_string());
        headers.add("To", "<sip:alice@example.com>".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());
        headers.add("CSeq", "1 REGISTER".to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.starts_with("REGISTER sip:registrar.example.com SIP/2.0\r\n"));
        assert!(output.contains("Via: SIP/2.0/UDP 10.0.0.1:5060\r\n"));
        assert!(output.contains("From: <sip:alice@example.com>;tag=1234\r\n"));
        assert!(output.contains("Call-ID: abc123@10.0.0.1\r\n"));
        assert!(output.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_format_invite_request_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: Some(body.clone()),
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("INVITE sip:bob@example.com SIP/2.0\r\n"));
        // Should auto-add Content-Length
        assert!(output.contains(&format!("Content-Length: {}\r\n", body.len())));
        // Body should be at the end after empty line
        assert!(bytes.ends_with(&body));
    }

    #[test]
    fn test_format_request_with_body_and_existing_content_length() {
        let body = b"test body".to_vec();
        let mut headers = Headers::new();
        headers.add("Content-Length", body.len().to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: Some(body.clone()),
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8_lossy(&bytes);

        // Should NOT duplicate Content-Length
        let cl_count = output.matches("Content-Length").count();
        assert_eq!(cl_count, 1, "Content-Length should appear exactly once");
    }

    #[test]
    fn test_format_request_no_headers_no_body() {
        let msg = SipMessage::Request(SipRequest {
            method: Method::Ack,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let bytes = format_sip_message(&msg);
        let expected = b"ACK sip:bob@example.com SIP/2.0\r\n\r\n";
        assert_eq!(bytes, expected);
    }

    #[test]
    fn test_format_request_other_method() {
        let msg = SipMessage::Request(SipRequest {
            method: Method::Other("SUBSCRIBE".to_string()),
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let bytes = format_sip_message(&msg);
        assert!(bytes.starts_with(b"SUBSCRIBE sip:bob@example.com SIP/2.0\r\n"));
    }

    // --- Unit tests: Response formatting ---

    #[test]
    fn test_format_200_ok_response_no_body() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(output.contains("Via: SIP/2.0/UDP 10.0.0.1:5060\r\n"));
        assert!(output.contains("Call-ID: abc123@10.0.0.1\r\n"));
        assert!(output.ends_with("\r\n\r\n"));
    }

    #[test]
    fn test_format_100_trying_response() {
        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 100,
            reason_phrase: "Trying".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let bytes = format_sip_message(&msg);
        let expected = b"SIP/2.0 100 Trying\r\n\r\n";
        assert_eq!(bytes, expected);
    }

    #[test]
    fn test_format_401_unauthorized_response() {
        let mut headers = Headers::new();
        headers.add(
            "WWW-Authenticate",
            "Digest realm=\"example.com\", nonce=\"abc123\"".to_string(),
        );

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8(bytes).unwrap();

        assert!(output.starts_with("SIP/2.0 401 Unauthorized\r\n"));
        assert!(output.contains("WWW-Authenticate: Digest realm=\"example.com\", nonce=\"abc123\"\r\n"));
    }

    #[test]
    fn test_format_response_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.2\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: Some(body.clone()),
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8_lossy(&bytes);

        assert!(output.starts_with("SIP/2.0 200 OK\r\n"));
        assert!(output.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(bytes.ends_with(&body));
    }

    #[test]
    fn test_format_response_with_body_and_existing_content_length() {
        let body = b"test".to_vec();
        let mut headers = Headers::new();
        headers.add("Content-Length", body.len().to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: Some(body),
        });

        let bytes = format_sip_message(&msg);
        let output = String::from_utf8_lossy(&bytes);

        let cl_count = output.matches("Content-Length").count();
        assert_eq!(cl_count, 1, "Content-Length should appear exactly once");
    }

    // --- Unit tests: Empty body edge case ---

    #[test]
    fn test_format_request_with_empty_body() {
        let msg = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: Some(vec![]),
        });

        let bytes = format_sip_message(&msg);
        // Empty body should not add Content-Length
        let output = String::from_utf8(bytes).unwrap();
        assert!(!output.contains("Content-Length"));
        assert!(output.ends_with("\r\n\r\n"));
    }

    // --- Unit tests: Roundtrip with parser ---

    #[test]
    fn test_format_then_parse_register_request() {
        use crate::sip::parser::parse_sip_message;

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("From", "<sip:alice@example.com>;tag=1234".to_string());
        headers.add("To", "<sip:alice@example.com>".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());
        headers.add("CSeq", "1 REGISTER".to_string());

        let original = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        let bytes = format_sip_message(&original);
        let parsed = parse_sip_message(&bytes).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_format_then_parse_200_ok_response() {
        use crate::sip::parser::parse_sip_message;

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());
        headers.add("CSeq", "1 INVITE".to_string());

        let original = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        let bytes = format_sip_message(&original);
        let parsed = parse_sip_message(&bytes).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_format_then_parse_request_with_body() {
        use crate::sip::parser::parse_sip_message;

        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("CSeq", "1 INVITE".to_string());

        let original = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: Some(body),
        });

        let bytes = format_sip_message(&original);
        let parsed = parse_sip_message(&bytes).unwrap();

        // The parsed version will have Content-Length header added by formatter
        // so we compare the key fields
        match (&original, &parsed) {
            (SipMessage::Request(orig), SipMessage::Request(pars)) => {
                assert_eq!(orig.method, pars.method);
                assert_eq!(orig.request_uri, pars.request_uri);
                assert_eq!(orig.version, pars.version);
                assert_eq!(orig.body, pars.body);
            }
            _ => panic!("Expected both to be Request"),
        }
    }

    // --- Property-based test: SIP message roundtrip ---
    // Feature: sip-load-tester, Property 1: SIPメッセージのラウンドトリップ
    // **Validates: Requirements 1.1, 1.2, 1.4, 1.5**

    // --- Unit tests: estimate_message_size ---

    #[test]
    fn test_estimate_message_size_request_no_body() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        let estimated = estimate_message_size(&msg);
        let actual = format_sip_message(&msg).len();
        // Estimate should be >= actual (no under-allocation)
        assert!(
            estimated >= actual,
            "estimate {} should be >= actual {}",
            estimated,
            actual
        );
    }

    #[test]
    fn test_estimate_message_size_response_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.2\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: Some(body),
        });

        let estimated = estimate_message_size(&msg);
        let actual = format_sip_message(&msg).len();
        assert!(
            estimated >= actual,
            "estimate {} should be >= actual {}",
            estimated,
            actual
        );
    }

    // --- Unit tests: format_into ---

    #[test]
    fn test_format_into_produces_same_output_as_format_sip_message() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("From", "<sip:alice@example.com>;tag=1234".to_string());
        headers.add("To", "<sip:alice@example.com>".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());
        headers.add("CSeq", "1 REGISTER".to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        let expected = format_sip_message(&msg);
        let mut buf = Vec::new();
        format_into(&mut buf, &msg);
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_format_into_response_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.2\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: Some(body),
        });

        let expected = format_sip_message(&msg);
        let mut buf = Vec::new();
        format_into(&mut buf, &msg);
        assert_eq!(buf, expected);
    }

    #[test]
    fn test_format_into_reuses_buffer() {
        let msg1 = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 100,
            reason_phrase: "Trying".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let msg2 = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let mut buf = Vec::new();

        // First message
        format_into(&mut buf, &msg1);
        assert_eq!(buf, format_sip_message(&msg1));

        // Reuse buffer for second message
        buf.clear();
        format_into(&mut buf, &msg2);
        assert_eq!(buf, format_sip_message(&msg2));
    }

    #[test]
    fn test_format_into_status_code_direct_write() {
        // Test various status codes to ensure itoa produces correct output
        for code in [100u16, 180, 200, 401, 404, 486, 500, 503] {
            let msg = SipMessage::Response(SipResponse {
                version: "SIP/2.0".to_string(),
                status_code: code,
                reason_phrase: "Test".to_string(),
                headers: Headers::new(),
                body: None,
            });

            let expected = format_sip_message(&msg);
            let mut buf = Vec::new();
            format_into(&mut buf, &msg);
            assert_eq!(
                buf, expected,
                "Mismatch for status code {}",
                code
            );
        }
    }

    // --- Unit tests: format_into_pooled ---

    #[test]
    fn test_format_into_pooled_invite_request() {
        use crate::sip::pool::MessageBuf;

        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("From", "<sip:alice@example.com>;tag=1234".to_string());
        headers.add("To", "<sip:bob@example.com>".to_string());
        headers.add("Call-ID", "invite123@10.0.0.1".to_string());
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: Some(body),
        });

        let expected = format_sip_message(&msg);
        let mut buf = MessageBuf::new();
        format_into_pooled(&mut buf, &msg);
        assert_eq!(buf.output, expected, "プール版INVITEフォーマットは既存版と等価であるべき");
    }

    #[test]
    fn test_format_into_pooled_200_ok_response() {
        use crate::sip::pool::MessageBuf;

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        headers.add("Call-ID", "abc123@10.0.0.1".to_string());
        headers.add("CSeq", "1 INVITE".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        let expected = format_sip_message(&msg);
        let mut buf = MessageBuf::new();
        format_into_pooled(&mut buf, &msg);
        assert_eq!(buf.output, expected, "プール版200 OKフォーマットは既存版と等価であるべき");
    }

    #[test]
    fn test_format_into_pooled_response_with_body() {
        use crate::sip::pool::MessageBuf;

        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.2\r\ns=session\r\n".to_vec();
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.2:5060".to_string());
        headers.add("CSeq", "1 INVITE".to_string());
        headers.add("Content-Type", "application/sdp".to_string());

        let msg = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: Some(body),
        });

        let expected = format_sip_message(&msg);
        let mut buf = MessageBuf::new();
        format_into_pooled(&mut buf, &msg);
        assert_eq!(buf.output, expected, "プール版ボディ付きフォーマットは既存版と等価であるべき");
    }

    #[test]
    fn test_format_into_pooled_reuses_output_buffer() {
        use crate::sip::pool::MessageBuf;

        let msg1 = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 100,
            reason_phrase: "Trying".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let msg2 = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let mut buf = MessageBuf::new();

        // 1回目のフォーマット
        format_into_pooled(&mut buf, &msg1);
        let expected1 = format_sip_message(&msg1);
        assert_eq!(buf.output, expected1, "1回目のフォーマット結果が正しいこと");
        let capacity_after_first = buf.output.capacity();

        // 2回目のフォーマット（buf.output が再利用される）
        format_into_pooled(&mut buf, &msg2);
        let expected2 = format_sip_message(&msg2);
        assert_eq!(buf.output, expected2, "2回目のフォーマット結果が正しいこと");
        assert!(
            buf.output.capacity() >= capacity_after_first,
            "2回連続フォーマットで capacity が維持されるべき (1回目: {}, 2回目: {})",
            capacity_after_first,
            buf.output.capacity()
        );
    }

    use crate::sip::message::generators::{arb_sip_message, arb_sip_request, arb_sip_response};
    use proptest::prelude::*;

    /// Normalize a SipMessage to account for format→parse transformations:
    /// - Trim header values (parser trims whitespace)
    /// - Trim leading whitespace from reason_phrase (parser's space1 consumes leading spaces)
    /// - Add Content-Length header for non-empty bodies when missing (formatter auto-adds)
    /// - Treat empty body (Some(vec![])) as None (formatter writes no body for empty vec)
    fn normalize_for_roundtrip(msg: &SipMessage) -> SipMessage {
        match msg {
            SipMessage::Request(req) => {
                let mut headers = Headers::new();
                for h in req.headers.entries() {
                    headers.add(&h.name, h.value.trim().to_string());
                }
                // Normalize body: empty vec → None
                let body = match &req.body {
                    Some(b) if !b.is_empty() => Some(b.clone()),
                    _ => None,
                };
                // Add Content-Length if body is present and header is missing
                if let Some(ref b) = body {
                    if headers.get("Content-Length").is_none() {
                        headers.add("Content-Length", b.len().to_string());
                    }
                }
                SipMessage::Request(SipRequest {
                    method: req.method.clone(),
                    request_uri: req.request_uri.clone(),
                    version: req.version.clone(),
                    headers,
                    body,
                })
            }
            SipMessage::Response(resp) => {
                let mut headers = Headers::new();
                for h in resp.headers.entries() {
                    headers.add(&h.name, h.value.trim().to_string());
                }
                let body = match &resp.body {
                    Some(b) if !b.is_empty() => Some(b.clone()),
                    _ => None,
                };
                if let Some(ref b) = body {
                    if headers.get("Content-Length").is_none() {
                        headers.add("Content-Length", b.len().to_string());
                    }
                }
                // Trim leading whitespace from reason_phrase: the parser's space1
                // between status_code and reason_phrase greedily consumes all leading spaces
                let reason_phrase = resp.reason_phrase.trim_start().to_string();
                SipMessage::Response(SipResponse {
                    version: resp.version.clone(),
                    status_code: resp.status_code,
                    reason_phrase,
                    headers,
                    body,
                })
            }
        }
    }

    proptest! {
        #[test]
        fn prop_sip_message_roundtrip(msg in arb_sip_message()) {
            // Feature: sip-load-tester, Property 1: SIPメッセージのラウンドトリップ
            // **Validates: Requirements 1.1, 1.2, 1.4, 1.5**
            use crate::sip::parser::parse_sip_message as parse_msg;

            let normalized = normalize_for_roundtrip(&msg);
            let bytes = format_sip_message(&normalized);
            let parsed = parse_msg(&bytes).expect("format→parse should succeed for any valid SipMessage");
            prop_assert_eq!(normalized, parsed);
        }

        #[test]
        fn prop_sip_request_roundtrip(req in arb_sip_request()) {
            // Feature: sip-load-tester, Property 1: SIPメッセージのラウンドトリップ (request variant)
            // **Validates: Requirements 1.1, 1.4, 1.5**
            use crate::sip::parser::parse_sip_message as parse_msg;

            let msg = SipMessage::Request(req);
            let normalized = normalize_for_roundtrip(&msg);
            let bytes = format_sip_message(&normalized);
            let parsed = parse_msg(&bytes).expect("format→parse should succeed for any valid SipRequest");
            prop_assert_eq!(normalized, parsed);
        }

        #[test]
        fn prop_sip_response_roundtrip(resp in arb_sip_response()) {
            // Feature: sip-load-tester, Property 1: SIPメッセージのラウンドトリップ (response variant)
            // **Validates: Requirements 1.2, 1.4, 1.5**
            use crate::sip::parser::parse_sip_message as parse_msg;

            let msg = SipMessage::Response(resp);
            let normalized = normalize_for_roundtrip(&msg);
            let bytes = format_sip_message(&normalized);
            let parsed = parse_msg(&bytes).expect("format→parse should succeed for any valid SipResponse");
            prop_assert_eq!(normalized, parsed);
        }

        /// Feature: performance-bottleneck-optimization, Property 3: フォーマッター出力のバイト列一致
        /// For any valid SipMessage, format_into(&mut buf, msg) produces byte-identical output
        /// to format_sip_message(msg).
        /// **Validates: Requirements 2.3**
        #[test]
        fn prop_format_into_byte_identical(msg in arb_sip_message()) {
            let expected = format_sip_message(&msg);
            let mut buf = Vec::new();
            format_into(&mut buf, &msg);
            prop_assert_eq!(buf, expected, "format_into must produce byte-identical output to format_sip_message");
        }

        /// Feature: sip-message-pool, Property 6: プール版フォーマッターの機能的等価性
        /// For any valid SipMessage, format_into_pooled(&mut buf, &msg) produces
        /// byte-identical output to format_sip_message(&msg).
        /// **Validates: Requirements 3.1, 3.2**
        #[test]
        fn prop_format_into_pooled_byte_identical(msg in arb_sip_message()) {
            use crate::sip::pool::MessageBuf;

            let expected = format_sip_message(&msg);
            let mut buf = MessageBuf::new();
            format_into_pooled(&mut buf, &msg);
            prop_assert_eq!(buf.output, expected, "format_into_pooled must produce byte-identical output to format_sip_message");
        }
    }
}
