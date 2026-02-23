// SIP message parser using nom combinators

use nom::{
    bytes::complete::{tag, take_until, take_while1},
    character::complete::{digit1, space0, space1},
    IResult,
};
use std::fmt;

use super::message::{Headers, Method, SipMessage, SipRequest, SipResponse};

/// Parse error with descriptive messages
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SIP parse error: {}", self.message)
    }
}

impl std::error::Error for ParseError {}

impl ParseError {
    pub fn new(message: impl Into<String>) -> Self {
        ParseError {
            message: message.into(),
        }
    }
}

/// Parse a SIP message from raw bytes.
///
/// Determines whether the input is a request or response by checking
/// if the first line starts with "SIP/" (response) or not (request).
pub fn parse_sip_message(input: &[u8]) -> Result<SipMessage, ParseError> {
    if input.is_empty() {
        return Err(ParseError::new("empty input"));
    }

    if input.starts_with(b"SIP/") {
        parse_response(input)
    } else {
        parse_request(input)
    }
}

/// Parse a SIP method string into a Method enum
fn parse_method(method_str: &str) -> Method {
    match method_str {
        "REGISTER" => Method::Register,
        "INVITE" => Method::Invite,
        "ACK" => Method::Ack,
        "BYE" => Method::Bye,
        "OPTIONS" => Method::Options,
        "UPDATE" => Method::Update,
        other => Method::Other(other.to_string()),
    }
}


/// nom parser: consume a CRLF sequence
fn crlf(input: &[u8]) -> IResult<&[u8], &[u8]> {
    tag(b"\r\n")(input)
}

/// Parse the request line: METHOD SP Request-URI SP SIP-Version CRLF
fn parse_request_line(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8], &[u8])> {
    let (input, method) = take_while1(|b: u8| b.is_ascii_alphabetic())(input)?;
    let (input, _) = space1(input)?;
    let (input, uri) = take_while1(|b: u8| b != b' ' && b != b'\r' && b != b'\n')(input)?;
    let (input, _) = space1(input)?;
    let (input, version) = take_until("\r\n")(input)?;
    let (input, _) = crlf(input)?;
    Ok((input, (method, uri, version)))
}

/// Parse the status line: SIP-Version SP Status-Code SP Reason-Phrase CRLF
fn parse_status_line(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8], &[u8])> {
    let (input, version) = take_while1(|b: u8| b != b' ' && b != b'\r' && b != b'\n')(input)?;
    let (input, _) = space1(input)?;
    let (input, status_code) = digit1(input)?;
    let (input, _) = space1(input)?;
    let (input, reason) = take_until("\r\n")(input)?;
    let (input, _) = crlf(input)?;
    Ok((input, (version, status_code, reason)))
}

/// Parse a single header line: Header-Name: Header-Value CRLF
fn parse_header_line(input: &[u8]) -> IResult<&[u8], (&[u8], &[u8])> {
    let (input, name) = take_while1(|b: u8| b != b':' && b != b'\r' && b != b'\n')(input)?;
    let (input, _) = tag(b":")(input)?;
    let (input, _) = space0(input)?;
    let (input, value) = take_until("\r\n")(input)?;
    let (input, _) = crlf(input)?;
    Ok((input, (name, value)))
}

/// Parse all headers until an empty line (CRLF CRLF)
fn parse_headers(mut input: &[u8]) -> IResult<&[u8], Vec<(&[u8], &[u8])>> {
    let mut headers = Vec::new();
    loop {
        // Check for empty line (end of headers)
        if input.starts_with(b"\r\n") {
            let (remaining, _) = crlf(input)?;
            return Ok((remaining, headers));
        }
        if input.is_empty() {
            return Ok((input, headers));
        }
        let (remaining, header) = parse_header_line(input)?;
        headers.push(header);
        input = remaining;
    }
}

/// Parse a full SIP request message
fn parse_request(input: &[u8]) -> Result<SipMessage, ParseError> {
    let (remaining, (method_bytes, uri_bytes, version_bytes)) =
        parse_request_line(input).map_err(|e| ParseError::new(format!("invalid request line: {}", e)))?;

    let method_str = std::str::from_utf8(method_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in method"))?;
    let request_uri = std::str::from_utf8(uri_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in request URI"))?
        .to_string();
    let version = std::str::from_utf8(version_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in SIP version"))?
        .to_string();

    if !version.starts_with("SIP/") {
        return Err(ParseError::new(format!("invalid SIP version: {}", version)));
    }

    let method = parse_method(method_str);

    let (remaining, raw_headers) =
        parse_headers(remaining).map_err(|e| ParseError::new(format!("invalid headers: {}", e)))?;

    let mut headers = Headers::new();
    for (name_bytes, value_bytes) in &raw_headers {
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ParseError::new("invalid UTF-8 in header name"))?
            .trim()
            .to_string();
        let value = std::str::from_utf8(value_bytes)
            .map_err(|_| ParseError::new("invalid UTF-8 in header value"))?
            .trim()
            .to_string();
        headers.add(&name, value);
    }

    let body = parse_body(remaining, &headers)?;

    Ok(SipMessage::Request(SipRequest {
        method,
        request_uri,
        version,
        headers,
        body,
    }))
}

/// Parse a full SIP response message
fn parse_response(input: &[u8]) -> Result<SipMessage, ParseError> {
    let (remaining, (version_bytes, status_bytes, reason_bytes)) =
        parse_status_line(input).map_err(|e| ParseError::new(format!("invalid status line: {}", e)))?;

    let version = std::str::from_utf8(version_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in SIP version"))?
        .to_string();
    let status_str = std::str::from_utf8(status_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in status code"))?;
    let status_code: u16 = status_str
        .parse()
        .map_err(|_| ParseError::new(format!("invalid status code: {}", status_str)))?;
    let reason_phrase = std::str::from_utf8(reason_bytes)
        .map_err(|_| ParseError::new("invalid UTF-8 in reason phrase"))?
        .to_string();

    if !version.starts_with("SIP/") {
        return Err(ParseError::new(format!("invalid SIP version: {}", version)));
    }

    let (remaining, raw_headers) =
        parse_headers(remaining).map_err(|e| ParseError::new(format!("invalid headers: {}", e)))?;

    let mut headers = Headers::new();
    for (name_bytes, value_bytes) in &raw_headers {
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| ParseError::new("invalid UTF-8 in header name"))?
            .trim()
            .to_string();
        let value = std::str::from_utf8(value_bytes)
            .map_err(|_| ParseError::new("invalid UTF-8 in header value"))?
            .trim()
            .to_string();
        headers.add(&name, value);
    }

    let body = parse_body(remaining, &headers)?;

    Ok(SipMessage::Response(SipResponse {
        version,
        status_code,
        reason_phrase,
        headers,
        body,
    }))
}

/// Parse the message body based on Content-Length header
fn parse_body(remaining: &[u8], headers: &Headers) -> Result<Option<Vec<u8>>, ParseError> {
    let content_length = headers
        .get("Content-Length")
        .map(|v| {
            v.trim()
                .parse::<usize>()
                .map_err(|_| ParseError::new(format!("invalid Content-Length: {}", v)))
        })
        .transpose()?;

    match content_length {
        Some(0) => Ok(None),
        Some(len) => {
            if remaining.len() < len {
                Err(ParseError::new(format!(
                    "body too short: expected {} bytes, got {}",
                    len,
                    remaining.len()
                )))
            } else {
                Ok(Some(remaining[..len].to_vec()))
            }
        }
        None => {
            if remaining.is_empty() {
                Ok(None)
            } else {
                // No Content-Length but there's remaining data - treat as body
                Ok(Some(remaining.to_vec()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- Known SIP methods and "SIP/" prefix for filtering ---
    const KNOWN_METHODS: &[&[u8]] = &[
        b"REGISTER", b"INVITE", b"ACK", b"BYE", b"OPTIONS", b"UPDATE",
        b"SUBSCRIBE", b"NOTIFY", b"REFER", b"MESSAGE", b"INFO", b"PRACK",
        b"PUBLISH", b"CANCEL",
    ];

    /// Check if bytes could start a valid SIP request (starts with an ASCII-alpha method)
    fn starts_with_sip_method(data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }
        // A valid request line starts with alphabetic chars followed by a space
        KNOWN_METHODS.iter().any(|m| data.starts_with(m))
    }

    // Feature: sip-load-tester, Property 2: 不正SIPメッセージのエラー検出
    // **Validates: Requirements 1.3**
    proptest! {
        /// Random bytes that don't start with a valid SIP method or "SIP/" should return error
        #[test]
        fn prop_random_bytes_return_error(
            data in proptest::collection::vec(any::<u8>(), 0..100)
        ) {
            // Filter out sequences that accidentally start with a known SIP method or "SIP/"
            prop_assume!(!data.starts_with(b"SIP/"));
            prop_assume!(!starts_with_sip_method(&data));

            let result = parse_sip_message(&data);
            prop_assert!(result.is_err(), "random bytes should produce parse error, got: {:?}", result);
        }

        /// Truncated messages (valid request start but cut off before header terminator) should return error
        #[test]
        fn prop_truncated_request_returns_error(
            method in prop_oneof![
                Just("REGISTER"), Just("INVITE"), Just("ACK"),
                Just("BYE"), Just("OPTIONS"), Just("UPDATE"),
            ],
            uri in "[a-z]{1,10}:[a-z]{1,10}@[a-z]{1,10}\\.[a-z]{2,4}",
        ) {
            // Build a valid message, then cut within the first header line (before its CRLF)
            // This guarantees the message is always incomplete
            let request_line = format!("{} {} SIP/2.0\r\n", method, uri);
            let header_part = "Via: SIP/2.0/UDP 10.0.0.1:5060";
            // Truncate mid-header (no trailing CRLF) — parser must fail
            let mut truncated = request_line.into_bytes();
            truncated.extend_from_slice(header_part.as_bytes());

            let result = parse_sip_message(&truncated);
            prop_assert!(result.is_err(), "truncated request should produce parse error, got: {:?}", result);
        }

        /// Truncated responses (valid "SIP/" start but cut off mid-header) should return error
        #[test]
        fn prop_truncated_response_returns_error(
            status_code in 100u16..700,
            reason in "[A-Za-z]{1,20}",
        ) {
            // Build a valid status line, then cut within the first header line
            let status_line = format!("SIP/2.0 {} {}\r\n", status_code, reason);
            let header_part = "Via: SIP/2.0/UDP 10.0.0.1:5060";
            let mut truncated = status_line.into_bytes();
            truncated.extend_from_slice(header_part.as_bytes());

            let result = parse_sip_message(&truncated);
            prop_assert!(result.is_err(), "truncated response should produce parse error, got: {:?}", result);
        }

        /// Messages with invalid (non-numeric) status codes should return error
        #[test]
        fn prop_invalid_status_code_returns_error(
            bad_code in "[a-zA-Z]{1,5}",
        ) {
            let input = format!("SIP/2.0 {} Bad\r\n\r\n", bad_code);

            let result = parse_sip_message(input.as_bytes());
            prop_assert!(result.is_err(), "non-numeric status code should produce parse error, got: {:?}", result);
        }

        /// Messages missing the CRLF CRLF separator between headers and body should return error
        #[test]
        fn prop_missing_header_body_separator_returns_error(
            method in prop_oneof![
                Just("REGISTER"), Just("INVITE"), Just("BYE"), Just("OPTIONS"),
            ],
            uri in "[a-z]{1,10}:[a-z]{1,10}@[a-z]{1,10}\\.[a-z]{2,4}",
            header_value in "[a-zA-Z0-9/.: ]{1,30}",
        ) {
            // Build a message with a header but NO empty line (CRLF CRLF) terminator
            let input = format!("{} {} SIP/2.0\r\nVia: {}", method, uri, header_value);

            let result = parse_sip_message(input.as_bytes());
            prop_assert!(result.is_err(), "missing CRLF CRLF separator should produce parse error, got: {:?}", result);
        }

        /// Empty input should return error
        #[test]
        fn prop_empty_input_returns_error(
            // Use a dummy parameter to make proptest happy; we always test empty
            _dummy in Just(()),
        ) {
            let result = parse_sip_message(b"");
            prop_assert!(result.is_err(), "empty input should produce parse error");
        }
    }

    // --- Unit tests: Request parsing ---

    #[test]
    fn test_parse_register_request() {
        let input = b"REGISTER sip:registrar.example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       From: <sip:alice@example.com>;tag=1234\r\n\
                       To: <sip:alice@example.com>\r\n\
                       Call-ID: abc123@10.0.0.1\r\n\
                       CSeq: 1 REGISTER\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Register);
                assert_eq!(req.request_uri, "sip:registrar.example.com");
                assert_eq!(req.version, "SIP/2.0");
                assert_eq!(req.headers.get("Via"), Some("SIP/2.0/UDP 10.0.0.1:5060"));
                assert_eq!(req.headers.get("Call-ID"), Some("abc123@10.0.0.1"));
                assert!(req.body.is_none());
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_invite_request() {
        let input = b"INVITE sip:bob@example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       From: <sip:alice@example.com>;tag=5678\r\n\
                       To: <sip:bob@example.com>\r\n\
                       Call-ID: def456@10.0.0.1\r\n\
                       CSeq: 1 INVITE\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Invite);
                assert_eq!(req.request_uri, "sip:bob@example.com");
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_ack_request() {
        let input = b"ACK sip:bob@example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => assert_eq!(req.method, Method::Ack),
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_bye_request() {
        let input = b"BYE sip:bob@example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => assert_eq!(req.method, Method::Bye),
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_options_request() {
        let input = b"OPTIONS sip:proxy.example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => assert_eq!(req.method, Method::Options),
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_update_request() {
        let input = b"UPDATE sip:bob@example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => assert_eq!(req.method, Method::Update),
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_other_method_request() {
        let input = b"SUBSCRIBE sip:bob@example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Other("SUBSCRIBE".to_string()));
            }
            _ => panic!("Expected Request"),
        }
    }

    // --- Unit tests: Response parsing ---

    #[test]
    fn test_parse_200_ok_response() {
        let input = b"SIP/2.0 200 OK\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       From: <sip:alice@example.com>;tag=1234\r\n\
                       To: <sip:bob@example.com>;tag=5678\r\n\
                       Call-ID: abc123@10.0.0.1\r\n\
                       CSeq: 1 INVITE\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.version, "SIP/2.0");
                assert_eq!(resp.status_code, 200);
                assert_eq!(resp.reason_phrase, "OK");
                assert_eq!(resp.headers.get("Call-ID"), Some("abc123@10.0.0.1"));
                assert!(resp.body.is_none());
            }
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_parse_100_trying_response() {
        let input = b"SIP/2.0 100 Trying\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.status_code, 100);
                assert_eq!(resp.reason_phrase, "Trying");
            }
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_parse_401_unauthorized_response() {
        let input = b"SIP/2.0 401 Unauthorized\r\n\
                       WWW-Authenticate: Digest realm=\"example.com\", nonce=\"abc123\"\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.status_code, 401);
                assert_eq!(resp.reason_phrase, "Unauthorized");
                assert!(resp.headers.get("WWW-Authenticate").is_some());
            }
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_parse_404_not_found_response() {
        let input = b"SIP/2.0 404 Not Found\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.status_code, 404);
                assert_eq!(resp.reason_phrase, "Not Found");
            }
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_parse_481_response() {
        let input = b"SIP/2.0 481 Call/Transaction Does Not Exist\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.status_code, 481);
                assert_eq!(resp.reason_phrase, "Call/Transaction Does Not Exist");
            }
            _ => panic!("Expected Response"),
        }
    }

    // --- Unit tests: Header parsing ---

    #[test]
    fn test_parse_multiple_headers_same_name() {
        let input = b"REGISTER sip:registrar.example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP proxy1:5060\r\n\
                       Via: SIP/2.0/UDP proxy2:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                let vias = req.headers.get_all("Via");
                assert_eq!(vias.len(), 2);
                assert_eq!(vias[0], "SIP/2.0/UDP proxy1:5060");
                assert_eq!(vias[1], "SIP/2.0/UDP proxy2:5060");
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_header_with_whitespace_around_colon() {
        let input = b"REGISTER sip:registrar.example.com SIP/2.0\r\n\
                       Via:   SIP/2.0/UDP 10.0.0.1:5060  \r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert_eq!(req.headers.get("Via"), Some("SIP/2.0/UDP 10.0.0.1:5060"));
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_no_headers() {
        let input = b"BYE sip:bob@example.com SIP/2.0\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert!(req.headers.entries().is_empty());
            }
            _ => panic!("Expected Request"),
        }
    }

    // --- Unit tests: Body parsing ---

    #[test]
    fn test_parse_request_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.1\r\n";
        let content_length = body.len();
        let mut input = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Content-Length: {}\r\n\
             \r\n",
            content_length
        )
        .into_bytes();
        input.extend_from_slice(body);

        let result = parse_sip_message(&input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert_eq!(req.body, Some(body.to_vec()));
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_response_with_body() {
        let body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.2\r\n";
        let content_length = body.len();
        let mut input = format!(
            "SIP/2.0 200 OK\r\n\
             Content-Length: {}\r\n\
             \r\n",
            content_length
        )
        .into_bytes();
        input.extend_from_slice(body);

        let result = parse_sip_message(&input).unwrap();
        match result {
            SipMessage::Response(resp) => {
                assert_eq!(resp.body, Some(body.to_vec()));
            }
            _ => panic!("Expected Response"),
        }
    }

    #[test]
    fn test_parse_content_length_zero_no_body() {
        let input = b"INVITE sip:bob@example.com SIP/2.0\r\n\
                       Content-Length: 0\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert!(req.body.is_none());
            }
            _ => panic!("Expected Request"),
        }
    }

    #[test]
    fn test_parse_no_content_length_no_body() {
        let input = b"REGISTER sip:registrar.example.com SIP/2.0\r\n\
                       Via: SIP/2.0/UDP 10.0.0.1:5060\r\n\
                       \r\n";

        let result = parse_sip_message(input).unwrap();
        match result {
            SipMessage::Request(req) => {
                assert!(req.body.is_none());
            }
            _ => panic!("Expected Request"),
        }
    }

    // --- Unit tests: Error cases ---

    #[test]
    fn test_parse_empty_input() {
        let result = parse_sip_message(b"");
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("empty input"));
    }

    #[test]
    fn test_parse_garbage_input() {
        let result = parse_sip_message(b"this is not a SIP message");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_incomplete_request_line() {
        let result = parse_sip_message(b"INVITE sip:bob@example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_missing_crlf_after_headers() {
        let result = parse_sip_message(b"INVITE sip:bob@example.com SIP/2.0\r\nVia: test");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_body_too_short() {
        let input = b"INVITE sip:bob@example.com SIP/2.0\r\n\
                       Content-Length: 100\r\n\
                       \r\n\
                       short";

        let result = parse_sip_message(input);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("body too short"));
    }

    #[test]
    fn test_parse_invalid_content_length() {
        let input = b"INVITE sip:bob@example.com SIP/2.0\r\n\
                       Content-Length: abc\r\n\
                       \r\n";

        let result = parse_sip_message(input);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("Content-Length"));
    }

    // --- Unit tests: ParseError ---

    #[test]
    fn test_parse_error_display() {
        let err = ParseError::new("test error");
        assert_eq!(err.to_string(), "SIP parse error: test error");
    }

    #[test]
    fn test_parse_error_clone_and_eq() {
        let err = ParseError::new("test");
        let cloned = err.clone();
        assert_eq!(err, cloned);
    }
}
