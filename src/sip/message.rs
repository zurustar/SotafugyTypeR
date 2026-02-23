// SIP message data model

/// SIP method types
#[derive(Debug, Clone, PartialEq)]
pub enum Method {
    Register,
    Invite,
    Ack,
    Bye,
    Options,
    Update,
    Other(String),
}

/// A single SIP header (name-value pair)
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Collection of SIP headers
#[derive(Debug, Clone, PartialEq)]
pub struct Headers {
    entries: Vec<Header>,
}

impl Headers {
    /// Create an empty Headers collection
    pub fn new() -> Self {
        Headers {
            entries: Vec::new(),
        }
    }

    /// Get the first header value matching the name (case-insensitive)
    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// Get all header values matching the name (case-insensitive)
    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
            .collect()
    }

    /// Set a header value, replacing any existing headers with the same name
    pub fn set(&mut self, name: &str, value: String) {
        self.remove(name);
        self.entries.push(Header {
            name: name.to_string(),
            value,
        });
    }

    /// Add a header value without removing existing headers with the same name
    pub fn add(&mut self, name: &str, value: String) {
        self.entries.push(Header {
            name: name.to_string(),
            value,
        });
    }

    /// Remove all headers matching the name (case-insensitive)
    pub fn remove(&mut self, name: &str) {
        self.entries
            .retain(|h| !h.name.eq_ignore_ascii_case(name));
    }

    /// Remove only the first header matching the name (case-insensitive)
    pub fn remove_first(&mut self, name: &str) {
        if let Some(pos) = self
            .entries
            .iter()
            .position(|h| h.name.eq_ignore_ascii_case(name))
        {
            self.entries.remove(pos);
        }
    }

    /// Insert a header at a specific position
    pub fn insert_at(&mut self, index: usize, name: &str, value: String) {
        let index = index.min(self.entries.len());
        self.entries.insert(
            index,
            Header {
                name: name.to_string(),
                value,
            },
        );
    }

    /// Get the underlying entries
    pub fn entries(&self) -> &[Header] {
        &self.entries
    }
}

impl Default for Headers {
    fn default() -> Self {
        Self::new()
    }
}

/// SIP request message
#[derive(Debug, Clone, PartialEq)]
pub struct SipRequest {
    pub method: Method,
    pub request_uri: String,
    pub version: String,
    pub headers: Headers,
    pub body: Option<Vec<u8>>,
}

/// SIP response message
#[derive(Debug, Clone, PartialEq)]
pub struct SipResponse {
    pub version: String,
    pub status_code: u16,
    pub reason_phrase: String,
    pub headers: Headers,
    pub body: Option<Vec<u8>>,
}

/// Top-level SIP message enum
#[derive(Debug, Clone, PartialEq)]
pub enum SipMessage {
    Request(SipRequest),
    Response(SipResponse),
}

#[cfg(test)]
pub mod generators {
    use super::*;
    use proptest::prelude::*;

    /// Strategy for generating valid SIP methods
    pub fn arb_method() -> impl Strategy<Value = Method> {
        prop_oneof![
            Just(Method::Register),
            Just(Method::Invite),
            Just(Method::Ack),
            Just(Method::Bye),
            Just(Method::Options),
            Just(Method::Update),
            "[A-Z]{3,10}".prop_map(Method::Other),
        ]
    }

    /// Strategy for generating valid SIP request URIs (sip:user@domain)
    pub fn arb_request_uri() -> impl Strategy<Value = String> {
        ("[a-z][a-z0-9]{0,7}", "[a-z][a-z0-9]{0,7}\\.[a-z]{2,4}")
            .prop_map(|(user, domain)| format!("sip:{}@{}", user, domain))
    }

    /// Strategy for generating a valid SIP version string
    pub fn arb_sip_version() -> impl Strategy<Value = String> {
        Just("SIP/2.0".to_string())
    }

    /// Strategy for generating a single valid SIP header
    pub fn arb_header() -> impl Strategy<Value = Header> {
        ("[A-Za-z][A-Za-z0-9-]{0,15}", "[^\r\n]{1,40}")
            .prop_map(|(name, value)| Header { name, value })
    }

    /// Strategy for generating a Headers collection (0..8 headers)
    pub fn arb_headers() -> impl Strategy<Value = Headers> {
        proptest::collection::vec(arb_header(), 0..8).prop_map(|entries| Headers { entries })
    }

    /// Strategy for generating an optional body
    pub fn arb_body() -> impl Strategy<Value = Option<Vec<u8>>> {
        prop_oneof![
            Just(None),
            proptest::collection::vec(any::<u8>(), 0..64).prop_map(Some),
        ]
    }

    /// Strategy for generating a valid SipRequest
    pub fn arb_sip_request() -> impl Strategy<Value = SipRequest> {
        (
            arb_method(),
            arb_request_uri(),
            arb_sip_version(),
            arb_headers(),
            arb_body(),
        )
            .prop_map(|(method, request_uri, version, headers, body)| SipRequest {
                method,
                request_uri,
                version,
                headers,
                body,
            })
    }

    /// Strategy for generating a valid SipResponse
    pub fn arb_sip_response() -> impl Strategy<Value = SipResponse> {
        (
            arb_sip_version(),
            100..700u16,
            "[A-Za-z ]{1,20}",
            arb_headers(),
            arb_body(),
        )
            .prop_map(|(version, status_code, reason_phrase, headers, body)| SipResponse {
                version,
                status_code,
                reason_phrase,
                headers,
                body,
            })
    }

    /// Strategy for generating a valid SipMessage (request or response)
    pub fn arb_sip_message() -> impl Strategy<Value = SipMessage> {
        prop_oneof![
            arb_sip_request().prop_map(SipMessage::Request),
            arb_sip_response().prop_map(SipMessage::Response),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::generators::*;
    use proptest::prelude::*;

    // --- Unit tests: Method ---

    #[test]
    fn test_method_clone_and_eq() {
        let methods = vec![
            Method::Register,
            Method::Invite,
            Method::Ack,
            Method::Bye,
            Method::Options,
            Method::Update,
            Method::Other("SUBSCRIBE".to_string()),
        ];
        for m in &methods {
            let cloned = m.clone();
            assert_eq!(m, &cloned);
        }
    }

    #[test]
    fn test_method_debug() {
        let m = Method::Invite;
        let debug_str = format!("{:?}", m);
        assert!(debug_str.contains("Invite"));
    }

    #[test]
    fn test_method_other_inequality() {
        assert_ne!(Method::Other("FOO".into()), Method::Other("BAR".into()));
    }

    // --- Unit tests: Header ---

    #[test]
    fn test_header_construction() {
        let h = Header {
            name: "Via".to_string(),
            value: "SIP/2.0/UDP 10.0.0.1:5060".to_string(),
        };
        assert_eq!(h.name, "Via");
        assert_eq!(h.value, "SIP/2.0/UDP 10.0.0.1:5060");
    }

    #[test]
    fn test_header_clone_and_eq() {
        let h = Header {
            name: "From".to_string(),
            value: "<sip:alice@example.com>".to_string(),
        };
        let cloned = h.clone();
        assert_eq!(h, cloned);
    }

    // --- Unit tests: Headers ---

    #[test]
    fn test_headers_new_is_empty() {
        let headers = Headers::new();
        assert!(headers.entries().is_empty());
    }

    #[test]
    fn test_headers_add_and_get() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        assert_eq!(
            headers.get("Via"),
            Some("SIP/2.0/UDP 10.0.0.1:5060")
        );
    }

    #[test]
    fn test_headers_get_case_insensitive() {
        let mut headers = Headers::new();
        headers.add("Content-Type", "application/sdp".to_string());
        assert_eq!(headers.get("content-type"), Some("application/sdp"));
        assert_eq!(headers.get("CONTENT-TYPE"), Some("application/sdp"));
    }

    #[test]
    fn test_headers_get_missing() {
        let headers = Headers::new();
        assert_eq!(headers.get("Via"), None);
    }

    #[test]
    fn test_headers_get_all_multiple() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP proxy1:5060".to_string());
        headers.add("Via", "SIP/2.0/UDP proxy2:5060".to_string());
        let all = headers.get_all("Via");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], "SIP/2.0/UDP proxy1:5060");
        assert_eq!(all[1], "SIP/2.0/UDP proxy2:5060");
    }

    #[test]
    fn test_headers_set_replaces_existing() {
        let mut headers = Headers::new();
        headers.add("From", "old-value".to_string());
        headers.add("From", "another-old".to_string());
        headers.set("From", "new-value".to_string());
        let all = headers.get_all("From");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "new-value");
    }

    #[test]
    fn test_headers_remove() {
        let mut headers = Headers::new();
        headers.add("Via", "value1".to_string());
        headers.add("From", "value2".to_string());
        headers.add("Via", "value3".to_string());
        headers.remove("Via");
        assert_eq!(headers.get("Via"), None);
        assert_eq!(headers.get("From"), Some("value2"));
    }

    #[test]
    fn test_headers_remove_case_insensitive() {
        let mut headers = Headers::new();
        headers.add("Content-Type", "application/sdp".to_string());
        headers.remove("content-type");
        assert_eq!(headers.get("Content-Type"), None);
    }

    #[test]
    fn test_headers_remove_first_removes_only_first() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP proxy:5060".to_string());
        headers.add("Via", "SIP/2.0/UDP uac:5060".to_string());
        headers.remove_first("Via");
        let all = headers.get_all("Via");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "SIP/2.0/UDP uac:5060");
    }

    #[test]
    fn test_headers_remove_first_case_insensitive() {
        let mut headers = Headers::new();
        headers.add("Via", "value1".to_string());
        headers.add("Via", "value2".to_string());
        headers.remove_first("via");
        let all = headers.get_all("Via");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], "value2");
    }

    #[test]
    fn test_headers_remove_first_noop_when_missing() {
        let mut headers = Headers::new();
        headers.add("From", "value".to_string());
        headers.remove_first("Via");
        assert_eq!(headers.entries().len(), 1);
    }

    #[test]
    fn test_headers_insert_at_beginning() {
        let mut headers = Headers::new();
        headers.add("From", "alice".to_string());
        headers.add("To", "bob".to_string());
        headers.insert_at(0, "Via", "proxy-via".to_string());
        assert_eq!(headers.entries()[0].name, "Via");
        assert_eq!(headers.entries()[0].value, "proxy-via");
        assert_eq!(headers.entries()[1].name, "From");
    }

    #[test]
    fn test_headers_insert_at_end() {
        let mut headers = Headers::new();
        headers.add("Via", "uac-via".to_string());
        headers.insert_at(100, "From", "alice".to_string());
        assert_eq!(headers.entries().len(), 2);
        assert_eq!(headers.entries()[1].name, "From");
    }

    #[test]
    fn test_headers_insert_at_middle() {
        let mut headers = Headers::new();
        headers.add("Via", "first".to_string());
        headers.add("From", "alice".to_string());
        headers.insert_at(1, "To", "bob".to_string());
        assert_eq!(headers.entries()[0].name, "Via");
        assert_eq!(headers.entries()[1].name, "To");
        assert_eq!(headers.entries()[2].name, "From");
    }

    // --- Unit tests: SipRequest ---

    #[test]
    fn test_sip_request_construction() {
        let req = SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        };
        assert_eq!(req.method, Method::Invite);
        assert_eq!(req.request_uri, "sip:bob@example.com");
        assert_eq!(req.version, "SIP/2.0");
        assert!(req.body.is_none());
    }

    #[test]
    fn test_sip_request_with_body() {
        let body = b"v=0\r\n".to_vec();
        let req = SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: Some(body.clone()),
        };
        assert_eq!(req.body, Some(body));
    }

    #[test]
    fn test_sip_request_clone_and_eq() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060".to_string());
        let req = SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        };
        let cloned = req.clone();
        assert_eq!(req, cloned);
    }

    // --- Unit tests: SipResponse ---

    #[test]
    fn test_sip_response_construction() {
        let resp = SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers: Headers::new(),
            body: None,
        };
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");
    }

    #[test]
    fn test_sip_response_clone_and_eq() {
        let resp = SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 404,
            reason_phrase: "Not Found".to_string(),
            headers: Headers::new(),
            body: None,
        };
        let cloned = resp.clone();
        assert_eq!(resp, cloned);
    }

    // --- Unit tests: SipMessage ---

    #[test]
    fn test_sip_message_request_variant() {
        let req = SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        };
        let msg = SipMessage::Request(req.clone());
        if let SipMessage::Request(inner) = &msg {
            assert_eq!(inner, &req);
        } else {
            panic!("Expected Request variant");
        }
    }

    #[test]
    fn test_sip_message_response_variant() {
        let resp = SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers: Headers::new(),
            body: None,
        };
        let msg = SipMessage::Response(resp.clone());
        if let SipMessage::Response(inner) = &msg {
            assert_eq!(inner, &resp);
        } else {
            panic!("Expected Response variant");
        }
    }

    #[test]
    fn test_sip_message_clone_and_eq() {
        let msg = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:alice@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });
        let cloned = msg.clone();
        assert_eq!(msg, cloned);
    }

    // --- proptest: generators produce valid values ---

    proptest! {
        #[test]
        fn prop_arb_method_is_valid(m in arb_method()) {
            // All generated methods should be Debug-printable and cloneable
            let _ = format!("{:?}", m);
            let cloned = m.clone();
            prop_assert_eq!(m, cloned);
        }

        #[test]
        fn prop_arb_request_uri_starts_with_sip(uri in arb_request_uri()) {
            prop_assert!(uri.starts_with("sip:"));
            prop_assert!(uri.contains('@'));
        }

        #[test]
        fn prop_arb_sip_request_roundtrip_clone(req in arb_sip_request()) {
            let cloned = req.clone();
            prop_assert_eq!(req, cloned);
        }

        #[test]
        fn prop_arb_sip_response_roundtrip_clone(resp in arb_sip_response()) {
            let cloned = resp.clone();
            prop_assert_eq!(resp, cloned);
        }

        #[test]
        fn prop_arb_sip_message_roundtrip_clone(msg in arb_sip_message()) {
            let cloned = msg.clone();
            prop_assert_eq!(msg, cloned);
        }

        #[test]
        fn prop_arb_sip_response_status_code_in_range(resp in arb_sip_response()) {
            prop_assert!(resp.status_code >= 100 && resp.status_code < 700);
        }

        #[test]
        fn prop_arb_sip_request_version_is_sip20(req in arb_sip_request()) {
            prop_assert_eq!(req.version, "SIP/2.0");
        }

        #[test]
        fn prop_headers_add_then_get(
            name in "[A-Za-z][A-Za-z0-9-]{0,15}",
            value in "[^\r\n]{1,40}"
        ) {
            let mut headers = Headers::new();
            headers.add(&name, value.clone());
            let got = headers.get(&name);
            prop_assert_eq!(got, Some(value.as_str()));
        }

        #[test]
        fn prop_headers_set_replaces(
            name in "[A-Za-z][A-Za-z0-9-]{0,15}",
            v1 in "[^\r\n]{1,20}",
            v2 in "[^\r\n]{1,20}"
        ) {
            let mut headers = Headers::new();
            headers.add(&name, v1);
            headers.set(&name, v2.clone());
            let all = headers.get_all(&name);
            prop_assert_eq!(all.len(), 1);
            prop_assert_eq!(all[0], v2.as_str());
        }

        #[test]
        fn prop_headers_remove_clears(
            name in "[A-Za-z][A-Za-z0-9-]{0,15}",
            value in "[^\r\n]{1,40}"
        ) {
            let mut headers = Headers::new();
            headers.add(&name, value);
            headers.remove(&name);
            prop_assert_eq!(headers.get(&name), None);
        }
    }
}
