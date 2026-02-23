// Digest authentication module

use std::sync::Arc;

use md5::{Md5, Digest};
use rand::Rng;

use crate::error::SipLoadTestError;
use crate::sip::message::SipMessage;
use crate::user_pool::{UserEntry, UserPool};

/// Parsed authentication challenge from a 401/407 response
#[derive(Debug, Clone, PartialEq)]
pub struct AuthChallenge {
    pub realm: String,
    pub nonce: String,
    pub algorithm: String,
}

/// UAC-side Digest authentication
pub struct DigestAuth {
    pub user_pool: Arc<UserPool>,
}

impl DigestAuth {
    /// Create a new DigestAuth with a reference to the shared UserPool
    pub fn new(user_pool: Arc<UserPool>) -> Self {
        Self { user_pool }
    }

    /// Extract authentication challenge from a 401/407 SIP response.
    /// Looks for WWW-Authenticate or Proxy-Authenticate headers.
    pub fn parse_challenge(response: &SipMessage) -> Result<AuthChallenge, SipLoadTestError> {
        let headers = match response {
            SipMessage::Response(resp) => &resp.headers,
            SipMessage::Request(_) => {
                return Err(SipLoadTestError::AuthenticationFailed(
                    "Expected a SIP response, got a request".to_string(),
                ));
            }
        };

        // Try WWW-Authenticate first, then Proxy-Authenticate
        let auth_value = headers
            .get("WWW-Authenticate")
            .or_else(|| headers.get("Proxy-Authenticate"))
            .ok_or_else(|| {
                SipLoadTestError::AuthenticationFailed(
                    "No WWW-Authenticate or Proxy-Authenticate header found".to_string(),
                )
            })?;

        Self::parse_digest_params(auth_value)
    }

    /// Parse Digest authentication parameters from a header value.
    /// Expected format: Digest realm="...", nonce="...", algorithm=MD5
    fn parse_digest_params(header_value: &str) -> Result<AuthChallenge, SipLoadTestError> {
        let value = header_value.trim();
        if !value.starts_with("Digest ") && !value.starts_with("digest ") {
            return Err(SipLoadTestError::AuthenticationFailed(
                format!("Expected Digest scheme, got: {}", value),
            ));
        }

        let params_str = &value[7..]; // Skip "Digest "
        let mut realm = None;
        let mut nonce = None;
        let mut algorithm = "MD5".to_string(); // Default per RFC 2617

        for param in Self::split_params(params_str) {
            let param = param.trim();
            if let Some((key, val)) = param.split_once('=') {
                let key = key.trim().to_lowercase();
                let val = val.trim().trim_matches('"');
                match key.as_str() {
                    "realm" => realm = Some(val.to_string()),
                    "nonce" => nonce = Some(val.to_string()),
                    "algorithm" => algorithm = val.to_string(),
                    _ => {} // Ignore unknown parameters
                }
            }
        }

        let realm = realm.ok_or_else(|| {
            SipLoadTestError::AuthenticationFailed("Missing realm parameter".to_string())
        })?;
        let nonce = nonce.ok_or_else(|| {
            SipLoadTestError::AuthenticationFailed("Missing nonce parameter".to_string())
        })?;

        Ok(AuthChallenge {
            realm,
            nonce,
            algorithm,
        })
    }

    /// Split comma-separated parameters, respecting quoted strings
    fn split_params(s: &str) -> Vec<&str> {
        let mut result = Vec::new();
        let mut start = 0;
        let mut in_quotes = false;

        for (i, c) in s.char_indices() {
            match c {
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => {
                    result.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        if start < s.len() {
            result.push(&s[start..]);
        }
        result
    }

    /// Compute MD5 Digest response per RFC 2617:
    /// HA1 = MD5(username:realm:password)
    /// HA2 = MD5(method:digest-uri)
    /// response = MD5(HA1:nonce:HA2)
    pub fn compute_response(
        username: &str,
        password: &str,
        challenge: &AuthChallenge,
        method: &str,
        digest_uri: &str,
    ) -> String {
        let ha1 = md5_hex(&format!("{}:{}:{}", username, challenge.realm, password));
        let ha2 = md5_hex(&format!("{}:{}", method, digest_uri));
        md5_hex(&format!("{}:{}:{}", ha1, challenge.nonce, ha2))
    }

    /// Create an Authorization or Proxy-Authorization header value
    /// using user credentials from the UserPool.
    /// Format: Digest username="...", realm="...", nonce="...", uri="...", response="...", algorithm=MD5
    pub fn create_authorization_header(
        &self,
        user: &UserEntry,
        challenge: &AuthChallenge,
        method: &str,
        digest_uri: &str,
    ) -> String {
        let response = Self::compute_response(
            &user.username,
            &user.password,
            challenge,
            method,
            digest_uri,
        );

        format!(
            "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
            user.username, challenge.realm, challenge.nonce, digest_uri, response
        )
    }
}

/// Proxy-side authentication: generates challenges and verifies responses
pub struct ProxyAuth {
    realm: String,
    user_pool: Arc<UserPool>,
}

impl ProxyAuth {
    /// Create a new ProxyAuth with the given realm and shared UserPool
    pub fn new(realm: String, user_pool: Arc<UserPool>) -> Self {
        Self { realm, user_pool }
    }

    /// Generate an authentication challenge with a random nonce
    pub fn create_challenge(&self) -> AuthChallenge {
        let mut rng = rand::thread_rng();
        let nonce_bytes: [u8; 16] = rng.gen();
        let nonce = nonce_bytes.iter().map(|b| format!("{:02x}", b)).collect::<String>();

        AuthChallenge {
            realm: self.realm.clone(),
            nonce,
            algorithm: "MD5".to_string(),
        }
    }

    /// Verify an Authorization or Proxy-Authorization header in the request.
    /// Looks up the user in UserPool via find_by_username and computes the expected digest.
    pub fn verify(&self, request: &SipMessage) -> bool {
        let (headers, method, _request_uri) = match request {
            SipMessage::Request(req) => (&req.headers, &req.method, &req.request_uri),
            SipMessage::Response(_) => return false,
        };

        // Try Proxy-Authorization first, then Authorization
        let auth_value = match headers
            .get("Proxy-Authorization")
            .or_else(|| headers.get("Authorization"))
        {
            Some(v) => v,
            None => return false,
        };

        // Parse the Digest parameters
        let auth_value = auth_value.trim();
        if !auth_value.starts_with("Digest ") && !auth_value.starts_with("digest ") {
            return false;
        }

        let params_str = &auth_value[7..];
        let mut username = None;
        let mut nonce = None;
        let mut response = None;
        let mut uri = None;

        for param in DigestAuth::split_params(params_str) {
            let param = param.trim();
            if let Some((key, val)) = param.split_once('=') {
                let key = key.trim().to_lowercase();
                let val = val.trim().trim_matches('"');
                match key.as_str() {
                    "username" => username = Some(val.to_string()),
                    "nonce" => nonce = Some(val.to_string()),
                    "response" => response = Some(val.to_string()),
                    "uri" => uri = Some(val.to_string()),
                    _ => {}
                }
            }
        }

        let (username, nonce, response, uri) =
            match (username, nonce, response, uri) {
                (Some(u), Some(n), Some(r), Some(d)) => (u, n, r, d),
                _ => return false,
            };

        // Look up user in UserPool
        let user = match self.user_pool.find_by_username(&username) {
            Some(u) => u,
            None => return false,
        };

        // Compute expected digest
        let method_str = match method {
            crate::sip::message::Method::Register => "REGISTER",
            crate::sip::message::Method::Invite => "INVITE",
            crate::sip::message::Method::Ack => "ACK",
            crate::sip::message::Method::Bye => "BYE",
            crate::sip::message::Method::Options => "OPTIONS",
            crate::sip::message::Method::Update => "UPDATE",
            crate::sip::message::Method::Other(ref s) => s.as_str(),
        };

        let challenge = AuthChallenge {
            realm: self.realm.clone(),
            nonce,
            algorithm: "MD5".to_string(),
        };

        let expected = DigestAuth::compute_response(
            &user.username,
            &user.password,
            &challenge,
            method_str,
            &uri,
        );

        expected == response
    }
}

/// Compute MD5 hash and return as lowercase hex string
fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:032x}", result)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, SipResponse, SipRequest, Method};
    use crate::user_pool::{UsersFile, UserPool};

    // --- Helper functions ---

    fn make_user_pool() -> Arc<UserPool> {
        let users_file = UsersFile {
            users: vec![
                UserEntry {
                    username: "alice".to_string(),
                    domain: "example.com".to_string(),
                    password: "secret123".to_string(),
                },
                UserEntry {
                    username: "bob".to_string(),
                    domain: "example.com".to_string(),
                    password: "password456".to_string(),
                },
            ],
        };
        Arc::new(UserPool::from_users_file(users_file).unwrap())
    }

    fn make_401_response(realm: &str, nonce: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add(
            "WWW-Authenticate",
            format!("Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5", realm, nonce),
        );
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        })
    }

    fn make_407_response(realm: &str, nonce: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add(
            "Proxy-Authenticate",
            format!("Digest realm=\"{}\", nonce=\"{}\", algorithm=MD5", realm, nonce),
        );
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 407,
            reason_phrase: "Proxy Authentication Required".to_string(),
            headers,
            body: None,
        })
    }

    // --- DigestAuth::new ---

    #[test]
    fn new_creates_digest_auth_with_user_pool() {
        let pool = make_user_pool();
        let auth = DigestAuth::new(pool.clone());
        assert_eq!(auth.user_pool.len(), 2);
    }

    // --- parse_challenge: 401 WWW-Authenticate ---

    #[test]
    fn parse_challenge_extracts_from_401() {
        let response = make_401_response("example.com", "abc123");
        let challenge = DigestAuth::parse_challenge(&response).unwrap();
        assert_eq!(challenge.realm, "example.com");
        assert_eq!(challenge.nonce, "abc123");
        assert_eq!(challenge.algorithm, "MD5");
    }

    // --- parse_challenge: 407 Proxy-Authenticate ---

    #[test]
    fn parse_challenge_extracts_from_407() {
        let response = make_407_response("proxy.example.com", "xyz789");
        let challenge = DigestAuth::parse_challenge(&response).unwrap();
        assert_eq!(challenge.realm, "proxy.example.com");
        assert_eq!(challenge.nonce, "xyz789");
        assert_eq!(challenge.algorithm, "MD5");
    }

    // --- parse_challenge: default algorithm ---

    #[test]
    fn parse_challenge_defaults_algorithm_to_md5() {
        let mut headers = Headers::new();
        headers.add(
            "WWW-Authenticate",
            "Digest realm=\"test.com\", nonce=\"n1\"".to_string(),
        );
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let challenge = DigestAuth::parse_challenge(&response).unwrap();
        assert_eq!(challenge.algorithm, "MD5");
    }

    // --- parse_challenge: error cases ---

    #[test]
    fn parse_challenge_fails_on_request() {
        let request = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });
        let result = DigestAuth::parse_challenge(&request);
        assert!(result.is_err());
    }

    #[test]
    fn parse_challenge_fails_without_auth_header() {
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers: Headers::new(),
            body: None,
        });
        let result = DigestAuth::parse_challenge(&response);
        assert!(result.is_err());
    }

    #[test]
    fn parse_challenge_fails_on_non_digest_scheme() {
        let mut headers = Headers::new();
        headers.add("WWW-Authenticate", "Basic realm=\"test\"".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let result = DigestAuth::parse_challenge(&response);
        assert!(result.is_err());
    }

    #[test]
    fn parse_challenge_fails_without_realm() {
        let mut headers = Headers::new();
        headers.add("WWW-Authenticate", "Digest nonce=\"n1\"".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let result = DigestAuth::parse_challenge(&response);
        assert!(result.is_err());
    }

    #[test]
    fn parse_challenge_fails_without_nonce() {
        let mut headers = Headers::new();
        headers.add("WWW-Authenticate", "Digest realm=\"test.com\"".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let result = DigestAuth::parse_challenge(&response);
        assert!(result.is_err());
    }

    // --- parse_challenge: WWW-Authenticate takes priority over Proxy-Authenticate ---

    #[test]
    fn parse_challenge_prefers_www_authenticate() {
        let mut headers = Headers::new();
        headers.add(
            "WWW-Authenticate",
            "Digest realm=\"www.example.com\", nonce=\"www-nonce\"".to_string(),
        );
        headers.add(
            "Proxy-Authenticate",
            "Digest realm=\"proxy.example.com\", nonce=\"proxy-nonce\"".to_string(),
        );
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let challenge = DigestAuth::parse_challenge(&response).unwrap();
        assert_eq!(challenge.realm, "www.example.com");
        assert_eq!(challenge.nonce, "www-nonce");
    }

    // --- compute_response: RFC 2617 MD5 ---

    #[test]
    fn compute_response_follows_rfc2617() {
        // Manual calculation:
        // HA1 = MD5("alice:example.com:secret123")
        // HA2 = MD5("REGISTER:sip:example.com")
        // response = MD5(HA1:nonce123:HA2)
        let challenge = AuthChallenge {
            realm: "example.com".to_string(),
            nonce: "nonce123".to_string(),
            algorithm: "MD5".to_string(),
        };

        let result = DigestAuth::compute_response(
            "alice",
            "secret123",
            &challenge,
            "REGISTER",
            "sip:example.com",
        );

        // Verify by computing manually
        let ha1 = md5_hex("alice:example.com:secret123");
        let ha2 = md5_hex("REGISTER:sip:example.com");
        let expected = md5_hex(&format!("{}:nonce123:{}", ha1, ha2));

        assert_eq!(result, expected);
    }

    #[test]
    fn compute_response_returns_32_char_hex() {
        let challenge = AuthChallenge {
            realm: "test.com".to_string(),
            nonce: "n1".to_string(),
            algorithm: "MD5".to_string(),
        };
        let result = DigestAuth::compute_response("user", "pass", &challenge, "INVITE", "sip:test.com");
        assert_eq!(result.len(), 32);
        assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn compute_response_different_inputs_produce_different_outputs() {
        let challenge = AuthChallenge {
            realm: "example.com".to_string(),
            nonce: "nonce1".to_string(),
            algorithm: "MD5".to_string(),
        };
        let r1 = DigestAuth::compute_response("alice", "pass1", &challenge, "REGISTER", "sip:example.com");
        let r2 = DigestAuth::compute_response("bob", "pass2", &challenge, "REGISTER", "sip:example.com");
        assert_ne!(r1, r2);
    }

    #[test]
    fn compute_response_same_inputs_produce_same_output() {
        let challenge = AuthChallenge {
            realm: "example.com".to_string(),
            nonce: "nonce1".to_string(),
            algorithm: "MD5".to_string(),
        };
        let r1 = DigestAuth::compute_response("alice", "pass", &challenge, "REGISTER", "sip:example.com");
        let r2 = DigestAuth::compute_response("alice", "pass", &challenge, "REGISTER", "sip:example.com");
        assert_eq!(r1, r2);
    }

    // --- create_authorization_header ---

    #[test]
    fn create_authorization_header_format() {
        let pool = make_user_pool();
        let auth = DigestAuth::new(pool);
        let user = UserEntry {
            username: "alice".to_string(),
            domain: "example.com".to_string(),
            password: "secret123".to_string(),
        };
        let challenge = AuthChallenge {
            realm: "example.com".to_string(),
            nonce: "nonce123".to_string(),
            algorithm: "MD5".to_string(),
        };

        let header = auth.create_authorization_header(&user, &challenge, "REGISTER", "sip:example.com");

        assert!(header.starts_with("Digest "));
        assert!(header.contains("username=\"alice\""));
        assert!(header.contains("realm=\"example.com\""));
        assert!(header.contains("nonce=\"nonce123\""));
        assert!(header.contains("uri=\"sip:example.com\""));
        assert!(header.contains("algorithm=MD5"));
        assert!(header.contains("response=\""));
    }

    #[test]
    fn create_authorization_header_response_matches_compute_response() {
        let pool = make_user_pool();
        let auth = DigestAuth::new(pool);
        let user = UserEntry {
            username: "bob".to_string(),
            domain: "example.com".to_string(),
            password: "password456".to_string(),
        };
        let challenge = AuthChallenge {
            realm: "proxy.example.com".to_string(),
            nonce: "xyz789".to_string(),
            algorithm: "MD5".to_string(),
        };

        let header = auth.create_authorization_header(&user, &challenge, "INVITE", "sip:bob@example.com");
        let expected_response = DigestAuth::compute_response(
            "bob",
            "password456",
            &challenge,
            "INVITE",
            "sip:bob@example.com",
        );

        let response_part = format!("response=\"{}\"", expected_response);
        assert!(header.contains(&response_part));
    }

    // --- md5_hex helper ---

    #[test]
    fn md5_hex_known_value() {
        // MD5("") = d41d8cd98f00b204e9800998ecf8427e
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn md5_hex_known_value_abc() {
        // MD5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    // --- parse_challenge: edge cases with quoted values containing commas ---

    #[test]
    fn parse_challenge_handles_quoted_comma_in_realm() {
        let mut headers = Headers::new();
        headers.add(
            "WWW-Authenticate",
            "Digest realm=\"example,com\", nonce=\"n1\"".to_string(),
        );
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        });
        let challenge = DigestAuth::parse_challenge(&response).unwrap();
        assert_eq!(challenge.realm, "example,com");
    }

    // ===== ProxyAuth tests =====

    // --- ProxyAuth::new ---

    #[test]
    fn proxy_auth_new_stores_realm_and_user_pool() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool.clone());
        assert_eq!(proxy_auth.user_pool.len(), 2);
    }

    // --- ProxyAuth::create_challenge ---

    #[test]
    fn proxy_auth_create_challenge_uses_configured_realm() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("sip.example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();
        assert_eq!(challenge.realm, "sip.example.com");
    }

    #[test]
    fn proxy_auth_create_challenge_algorithm_is_md5() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();
        assert_eq!(challenge.algorithm, "MD5");
    }

    #[test]
    fn proxy_auth_create_challenge_nonce_is_nonempty() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();
        assert!(!challenge.nonce.is_empty());
    }

    #[test]
    fn proxy_auth_create_challenge_nonce_is_hex() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();
        assert!(challenge.nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn proxy_auth_create_challenge_generates_unique_nonces() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let c1 = proxy_auth.create_challenge();
        let c2 = proxy_auth.create_challenge();
        assert_ne!(c1.nonce, c2.nonce);
    }

    // --- ProxyAuth::verify ---

    /// Helper: build a SIP request with a Proxy-Authorization header
    fn make_request_with_proxy_auth(
        method: Method,
        request_uri: &str,
        auth_header: &str,
    ) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Proxy-Authorization", auth_header.to_string());
        SipMessage::Request(SipRequest {
            method,
            request_uri: request_uri.to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    /// Helper: build a SIP request with an Authorization header
    fn make_request_with_authorization(
        method: Method,
        request_uri: &str,
        auth_header: &str,
    ) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Authorization", auth_header.to_string());
        SipMessage::Request(SipRequest {
            method,
            request_uri: request_uri.to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    #[test]
    fn proxy_auth_verify_correct_credentials_returns_true() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();

        // alice is in the pool with password "secret123"
        let response_digest = DigestAuth::compute_response(
            "alice",
            "secret123",
            &challenge,
            "INVITE",
            "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            challenge.nonce, response_digest
        );
        let request = make_request_with_proxy_auth(
            Method::Invite,
            "sip:bob@example.com",
            &auth_header,
        );

        assert!(proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_wrong_password_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();

        // Compute with wrong password
        let response_digest = DigestAuth::compute_response(
            "alice",
            "wrongpassword",
            &challenge,
            "INVITE",
            "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            challenge.nonce, response_digest
        );
        let request = make_request_with_proxy_auth(
            Method::Invite,
            "sip:bob@example.com",
            &auth_header,
        );

        assert!(!proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_unknown_user_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();

        let response_digest = DigestAuth::compute_response(
            "unknown_user",
            "somepass",
            &challenge,
            "INVITE",
            "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"unknown_user\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            challenge.nonce, response_digest
        );
        let request = make_request_with_proxy_auth(
            Method::Invite,
            "sip:bob@example.com",
            &auth_header,
        );

        assert!(!proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_no_auth_header_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);

        let request = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });

        assert!(!proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_response_message_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);

        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers: Headers::new(),
            body: None,
        });

        assert!(!proxy_auth.verify(&response));
    }

    #[test]
    fn proxy_auth_verify_authorization_header_also_works() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);
        let challenge = proxy_auth.create_challenge();

        // Use Authorization header instead of Proxy-Authorization
        let response_digest = DigestAuth::compute_response(
            "bob",
            "password456",
            &challenge,
            "REGISTER",
            "sip:example.com",
        );
        let auth_header = format!(
            "Digest username=\"bob\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:example.com\", response=\"{}\", algorithm=MD5",
            challenge.nonce, response_digest
        );
        let request = make_request_with_authorization(
            Method::Register,
            "sip:example.com",
            &auth_header,
        );

        assert!(proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_malformed_auth_header_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);

        let request = make_request_with_proxy_auth(
            Method::Invite,
            "sip:bob@example.com",
            "not-a-valid-digest-header",
        );

        assert!(!proxy_auth.verify(&request));
    }

    #[test]
    fn proxy_auth_verify_missing_response_field_returns_false() {
        let pool = make_user_pool();
        let proxy_auth = ProxyAuth::new("example.com".to_string(), pool);

        // Auth header without response field
        let auth_header = "Digest username=\"alice\", realm=\"example.com\", nonce=\"abc\", uri=\"sip:bob@example.com\"";
        let request = make_request_with_proxy_auth(
            Method::Invite,
            "sip:bob@example.com",
            auth_header,
        );

        assert!(!proxy_auth.verify(&request));
    }

    // ===== Property-based tests =====

    use proptest::prelude::*;

    // Feature: sip-load-tester, Property 19: Digest認証レスポンス計算
    // **Validates: Requirements 20.1, 20.2, 20.3**
    proptest! {
        #[test]
        fn prop_digest_auth_response_matches_rfc2617(
            username in "[a-zA-Z0-9_]{1,20}",
            password in "[a-zA-Z0-9!@#$%^&*]{1,30}",
            realm in "[a-zA-Z0-9.]{1,30}",
            nonce in "[a-fA-F0-9]{8,32}",
            method in "(REGISTER|INVITE|BYE|ACK|OPTIONS|UPDATE)",
            digest_uri in "sip:[a-zA-Z0-9.@:]{1,40}",
        ) {
            let challenge = AuthChallenge {
                realm: realm.clone(),
                nonce: nonce.clone(),
                algorithm: "MD5".to_string(),
            };

            let result = DigestAuth::compute_response(
                &username,
                &password,
                &challenge,
                &method,
                &digest_uri,
            );

            // Manually compute expected per RFC 2617
            let ha1 = md5_hex(&format!("{}:{}:{}", username, realm, password));
            let ha2 = md5_hex(&format!("{}:{}", method, digest_uri));
            let expected = md5_hex(&format!("{}:{}:{}", ha1, nonce, ha2));

            prop_assert_eq!(result, expected);
        }
    }

    // Feature: sip-load-tester, Property 20: プロキシ認証検証
    // **Validates: Requirements 20.8**
    proptest! {
        #[test]
        fn prop_proxy_auth_verify_correct_digest_returns_true(
            username in "[a-zA-Z][a-zA-Z0-9_]{0,19}",
            password in "[a-zA-Z0-9]{1,30}",
            realm in "[a-zA-Z][a-zA-Z0-9.]{0,29}",
            nonce in "[a-fA-F0-9]{16,32}",
            method_idx in 0u8..6,
            uri_user in "[a-zA-Z][a-zA-Z0-9]{0,9}",
            uri_domain in "[a-zA-Z][a-zA-Z0-9]{0,9}\\.[a-z]{2,4}",
        ) {
            let (method, method_str) = match method_idx {
                0 => (Method::Register, "REGISTER"),
                1 => (Method::Invite, "INVITE"),
                2 => (Method::Bye, "BYE"),
                3 => (Method::Ack, "ACK"),
                4 => (Method::Options, "OPTIONS"),
                _ => (Method::Update, "UPDATE"),
            };
            let digest_uri = format!("sip:{}@{}", uri_user, uri_domain);

            // Build a UserPool containing the generated user
            let users_file = UsersFile {
                users: vec![UserEntry {
                    username: username.clone(),
                    domain: uri_domain.clone(),
                    password: password.clone(),
                }],
            };
            let pool = Arc::new(UserPool::from_users_file(users_file).unwrap());
            let proxy_auth = ProxyAuth::new(realm.clone(), pool);

            // Compute the correct digest response
            let challenge = AuthChallenge {
                realm: realm.clone(),
                nonce: nonce.clone(),
                algorithm: "MD5".to_string(),
            };
            let correct_response = DigestAuth::compute_response(
                &username, &password, &challenge, method_str, &digest_uri,
            );

            // Build request with correct Proxy-Authorization
            let auth_header = format!(
                "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
                username, realm, nonce, digest_uri, correct_response
            );
            let request = make_request_with_proxy_auth(method, &digest_uri, &auth_header);

            prop_assert!(proxy_auth.verify(&request), "verify should return true for correct digest");
        }

        // Feature: sip-load-tester, Property 25: UserPool経由のDigest認証整合性
        // **Validates: Requirements 20.3, 20.8（UserPool関連）**
        #[test]
        fn prop_user_pool_digest_auth_consistency(
            usernames in prop::collection::vec("[a-zA-Z][a-zA-Z0-9_]{0,14}", 2..10),
            passwords in prop::collection::vec("[a-zA-Z0-9]{1,20}", 2..10),
            domain in "[a-zA-Z][a-zA-Z0-9]{0,9}\\.[a-z]{2,4}",
            realm in "[a-zA-Z][a-zA-Z0-9.]{0,19}",
            nonce in "[a-fA-F0-9]{16,32}",
            method_idx in 0u8..6,
            uri_user in "[a-zA-Z][a-zA-Z0-9]{0,9}",
        ) {
            // Ensure we have matching counts and unique usernames
            let count = usernames.len().min(passwords.len());
            let mut seen = std::collections::HashSet::new();
            let users: Vec<UserEntry> = usernames.into_iter()
                .zip(passwords.into_iter())
                .take(count)
                .filter(|(u, _)| seen.insert(u.clone()))
                .map(|(username, password)| UserEntry {
                    username,
                    domain: domain.clone(),
                    password,
                })
                .collect();
            prop_assume!(!users.is_empty());

            let users_file = UsersFile { users: users.clone() };
            let pool = Arc::new(UserPool::from_users_file(users_file).unwrap());

            let proxy_auth = ProxyAuth::new(realm.clone(), pool.clone());
            let _digest_auth = DigestAuth::new(pool.clone());

            let (method, method_str) = match method_idx {
                0 => (Method::Register, "REGISTER"),
                1 => (Method::Invite, "INVITE"),
                2 => (Method::Bye, "BYE"),
                3 => (Method::Ack, "ACK"),
                4 => (Method::Options, "OPTIONS"),
                _ => (Method::Update, "UPDATE"),
            };
            let digest_uri = format!("sip:{}@{}", uri_user, domain);

            // For each user in the pool, compute digest and verify via ProxyAuth
            for user in &users {
                let challenge = AuthChallenge {
                    realm: realm.clone(),
                    nonce: nonce.clone(),
                    algorithm: "MD5".to_string(),
                };

                let response_digest = DigestAuth::compute_response(
                    &user.username,
                    &user.password,
                    &challenge,
                    method_str,
                    &digest_uri,
                );

                let auth_header = format!(
                    "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
                    user.username, realm, nonce, digest_uri, response_digest
                );
                let request = make_request_with_proxy_auth(
                    method.clone(),
                    &digest_uri,
                    &auth_header,
                );

                prop_assert!(
                    proxy_auth.verify(&request),
                    "ProxyAuth::verify should succeed for user '{}' from the same UserPool",
                    user.username
                );
            }
        }

        #[test]
        fn prop_proxy_auth_verify_incorrect_digest_returns_false(
            username in "[a-zA-Z][a-zA-Z0-9_]{0,19}",
            password in "[a-zA-Z0-9]{1,30}",
            realm in "[a-zA-Z][a-zA-Z0-9.]{0,29}",
            nonce in "[a-fA-F0-9]{16,32}",
            wrong_response in "[a-f0-9]{32}",
            uri_user in "[a-zA-Z][a-zA-Z0-9]{0,9}",
            uri_domain in "[a-zA-Z][a-zA-Z0-9]{0,9}\\.[a-z]{2,4}",
        ) {
            let digest_uri = format!("sip:{}@{}", uri_user, uri_domain);

            // Build a UserPool containing the generated user
            let users_file = UsersFile {
                users: vec![UserEntry {
                    username: username.clone(),
                    domain: uri_domain.clone(),
                    password: password.clone(),
                }],
            };
            let pool = Arc::new(UserPool::from_users_file(users_file).unwrap());
            let proxy_auth = ProxyAuth::new(realm.clone(), pool);

            // Compute the correct digest to filter out accidental matches
            let challenge = AuthChallenge {
                realm: realm.clone(),
                nonce: nonce.clone(),
                algorithm: "MD5".to_string(),
            };
            let correct_response = DigestAuth::compute_response(
                &username, &password, &challenge, "INVITE", &digest_uri,
            );

            // Skip if the random wrong_response accidentally matches
            prop_assume!(wrong_response != correct_response);

            // Build request with incorrect Proxy-Authorization
            let auth_header = format!(
                "Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{}\", response=\"{}\", algorithm=MD5",
                username, realm, nonce, digest_uri, wrong_response
            );
            let request = make_request_with_proxy_auth(
                Method::Invite, &digest_uri, &auth_header,
            );

            prop_assert!(!proxy_auth.verify(&request), "verify should return false for incorrect digest");
        }
    }
}
