// SIP Stateless Proxy module
//
// Implements a stateless SIP proxy that:
// - Adds its own Via header when forwarding requests
// - Removes its own Via header when forwarding responses
// - Inserts Record-Route header for INVITE requests
// - Routes in-dialog requests (ACK, BYE, re-INVITE) based on Route header
// - Maintains no transaction state (stateless operation)

use std::cell::RefCell;
use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::auth::ProxyAuth;
use crate::error::SipLoadTestError;
use crate::sip::formatter::format_into;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
use crate::uas::SipTransport;

thread_local! {
    static FMT_BUF: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(2048));
}

/// Thread-local buffer を使用して SipMessage をフォーマットし、バイト列を返す。
/// バッファは再利用されるため、毎回のヒープアロケーションを回避する。
fn format_message_reuse(msg: &SipMessage) -> Vec<u8> {
    FMT_BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        buf.clear();
        format_into(&mut buf, msg);
        buf.clone()
    })
}

/// Contact information stored in the Location Service.
#[derive(Debug, Clone)]
pub struct ContactInfo {
    pub contact_uri: String,
    pub address: SocketAddr,
    pub expires: Instant,
}

/// In-memory Location Service for REGISTER/INVITE routing.
/// Maps AOR (Address of Record) to ContactInfo.
pub struct LocationService {
    registrations: DashMap<String, ContactInfo>,
}

impl LocationService {
    /// Create a new empty LocationService.
    pub fn new() -> Self {
        Self {
            registrations: DashMap::new(),
        }
    }

    /// Register a contact for the given AOR.
    pub fn register(&self, aor: &str, contact: ContactInfo) {
        self.registrations.insert(aor.to_string(), contact);
    }

    /// Lookup a contact by AOR. Returns None if not registered.
    pub fn lookup(&self, aor: &str) -> Option<ContactInfo> {
        self.registrations.get(aor).map(|entry| entry.clone())
    }
}

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

/// Stateless SIP Proxy
pub struct SipProxy {
    transport: Arc<dyn SipTransport>,
    location_service: Arc<LocationService>,
    auth: Option<ProxyAuth>,
    config: ProxyConfig,
}

impl SipProxy {
    /// Create a new SipProxy instance.
    pub fn new(
        transport: Arc<dyn SipTransport>,
        location_service: Arc<LocationService>,
        auth: Option<ProxyAuth>,
        config: ProxyConfig,
    ) -> Self {
        Self {
            transport,
            location_service,
            auth,
            config,
        }
    }

    /// Handle an incoming SIP request: add Via, optionally add Record-Route,
    /// and forward to the destination.
    /// REGISTER: store in LocationService, respond 200 OK.
    /// INVITE without Route:
    ///   - If Request-URI targets the proxy itself → 404 (loop prevention)
    ///   - If LocationService has entry → forward to registered contact
    ///   - Otherwise → forward to default forward_addr
    /// When auth is enabled:
    ///   - INVITE without Proxy-Authorization → 407
    ///   - REGISTER without Authorization → 401
    ///   - Invalid auth → 403
    ///   - ACK, BYE (in-dialog) bypass auth
    pub async fn handle_request(
        &self,
        request: SipMessage,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        match request {
            SipMessage::Request(req) => {
                if self.config.debug {
                    let method = method_to_str(&req.method);
                    let uri = &req.request_uri;
                    let call_id = req.headers.get("Call-ID").unwrap_or("<unknown>");
                    eprintln!("{}", format_recv_req_log(method, uri, from, call_id));
                }

                if req.method == Method::Register {
                    // Auth check for REGISTER: 401 challenge
                    if let Some(ref auth) = self.auth {
                        let msg = SipMessage::Request(req.clone());
                        if !Self::has_auth_header(&req) {
                            // No auth header → send 401 challenge
                            let challenge = auth.create_challenge();
                            let mut response = self.build_response(&req, 401, "Unauthorized");
                            response.headers.add(
                                "WWW-Authenticate",
                                Self::format_challenge(&challenge),
                            );
                            let data = format_message_reuse(&SipMessage::Response(response));
                            self.transport.send_to(&data, from).await?;
                            return Ok(());
                        } else if !auth.verify(&msg) {
                            // Auth header present but invalid → 403
                            let response = self.build_response(&req, 403, "Forbidden");
                            let data = format_message_reuse(&SipMessage::Response(response));
                            self.transport.send_to(&data, from).await?;
                            return Ok(());
                        }
                        // Auth valid → proceed
                    }
                    self.handle_register(req, from).await
                } else if self.requires_auth(&req) {
                    // Auth check for INVITE (initial, without Route): 407 challenge
                    if let Some(ref auth) = self.auth {
                        let msg = SipMessage::Request(req.clone());
                        if !Self::has_proxy_auth_header(&req) {
                            // No Proxy-Authorization header → send 407 challenge
                            let challenge = auth.create_challenge();
                            let mut response = self.build_response(
                                &req,
                                407,
                                "Proxy Authentication Required",
                            );
                            response.headers.add(
                                "Proxy-Authenticate",
                                Self::format_challenge(&challenge),
                            );
                            let data = format_message_reuse(&SipMessage::Response(response));
                            self.transport.send_to(&data, from).await?;
                            return Ok(());
                        } else if !auth.verify(&msg) {
                            // Auth header present but invalid → 403
                            let response = self.build_response(&req, 403, "Forbidden");
                            let data = format_message_reuse(&SipMessage::Response(response));
                            self.transport.send_to(&data, from).await?;
                            return Ok(());
                        }
                        // Auth valid → proceed
                    }
                    self.forward_request(req, from).await
                } else {
                    // In-dialog requests (ACK, BYE with Route, etc.) bypass auth
                    self.forward_request(req, from).await
                }
            }
            SipMessage::Response(_) => Ok(()), // Handled by handle_response
        }
    }

    /// Handle an incoming SIP response: remove proxy's Via header and forward
    /// to the address from the next Via header.
    pub async fn handle_response(
        &self,
        response: SipMessage,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        match response {
            SipMessage::Response(resp) => {
                if self.config.debug {
                    let call_id = resp.headers.get("Call-ID").unwrap_or("<unknown>");
                    eprintln!("{}", format_recv_res_log(resp.status_code, &resp.reason_phrase, from, call_id));
                }
                self.forward_response(resp).await
            }
            SipMessage::Request(_) => Ok(()), // Ignore requests in response handler
        }
    }

    /// Check if the Request-URI targets this proxy's managed domain.
    /// Extracts the domain from URIs like "sip:user@domain" or "sip:domain".
    fn is_local_domain(&self, request_uri: &str) -> bool {
        // Strip "sip:" prefix
        let uri = request_uri.strip_prefix("sip:").unwrap_or(request_uri);
        // Extract domain part (after '@' if present, otherwise the whole thing)
        let domain = if let Some(at_pos) = uri.find('@') {
            &uri[at_pos + 1..]
        } else {
            uri
        };
        // Strip port if present
        let domain = domain.split(':').next().unwrap_or(domain);
        // Compare with managed domain
        domain == self.config.domain
    }

    /// Build a Via header value using push_str/write! to minimize format! usage.
    fn build_via_value(&self) -> String {
        let mut via = String::with_capacity(80);
        via.push_str("SIP/2.0/UDP ");
        via.push_str(&self.config.host);
        via.push(':');
        let _ = write!(via, "{}", self.config.port);
        via.push_str(";branch=z9hG4bK-proxy-");
        via.push_str(&rand_branch());
        via
    }

    /// Build a Record-Route header value using push_str/write! to minimize format! usage.
    fn build_record_route_value(&self) -> String {
        let mut rr = String::with_capacity(40);
        rr.push_str("<sip:");
        rr.push_str(&self.config.host);
        rr.push(':');
        let _ = write!(rr, "{}", self.config.port);
        rr.push_str(";lr>");
        rr
    }

    /// Forward a SIP request: add Via, add Record-Route for INVITE, determine destination.
    async fn forward_request(
        &self,
        mut request: SipRequest,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        // For INVITE without Route header, check if it targets our managed domain
        if request.method == Method::Invite && request.headers.get("Route").is_none() {
            if self.is_local_domain(&request.request_uri) {
                // Local domain: use LocationService
                match self.location_service.lookup(&request.request_uri) {
                    Some(contact) => {
                        // Forward to registered contact
                        request.request_uri = contact.contact_uri.clone();

                        request.headers.insert_at(0, "Via", self.build_via_value());
                        request.headers.insert_at(0, "Record-Route", self.build_record_route_value());

                        let dest = contact.address;
                        if self.config.debug {
                            let call_id = request.headers.get("Call-ID").unwrap_or("<unknown>");
                            let method = method_to_str(&request.method);
                            eprintln!("{}", format_fwd_req_log(method, call_id, dest));
                        }

                        let data = format_message_reuse(&SipMessage::Request(request));
                        if let Err(e) = self.transport.send_to(&data, dest).await {
                            if self.config.debug {
                                eprintln!("{}", format_req_error_log(&e.to_string(), "INVITE", "<unknown>", dest));
                            }
                            return Err(e);
                        }
                        return Ok(());
                    }
                    None => {
                        // User not registered in our domain → 404
                        let response = self.build_response(&request, 404, "Not Found");
                        let data = format_message_reuse(&SipMessage::Response(response));
                        self.transport.send_to(&data, from).await?;
                        return Ok(());
                    }
                }
            }
            // Non-local domain: fall through to normal forwarding below
        }

        // Add proxy's Via header at the top
        request.headers.insert_at(0, "Via", self.build_via_value());

        // For INVITE requests, insert Record-Route header
        if request.method == Method::Invite {
            request.headers.insert_at(0, "Record-Route", self.build_record_route_value());
        }

        // Determine forwarding destination
        let dest = self.determine_request_destination(&request, from);

        if self.config.debug {
            let call_id = request.headers.get("Call-ID").unwrap_or("<unknown>");
            let method = method_to_str(&request.method);
            eprintln!("{}", format_fwd_req_log(method, call_id, dest));
        }

        // Forward the request
        let data = format_message_reuse(&SipMessage::Request(request));
        if let Err(e) = self.transport.send_to(&data, dest).await {
            if self.config.debug {
                eprintln!("{}", format_req_error_log(&e.to_string(), "<unknown>", "<unknown>", dest));
            }
            return Err(e);
        }

        Ok(())
    }

    /// Determine where to forward a request.
    /// For in-dialog requests (ACK, BYE, re-INVITE with Route header), use Route header.
    /// Otherwise, use the default forward address.
    fn determine_request_destination(&self, request: &SipRequest, _from: SocketAddr) -> SocketAddr {
        // Check for Route header (in-dialog requests)
        if let Some(route) = request.headers.get("Route") {
            if let Some(addr) = parse_uri_addr(route) {
                return addr;
            }
        }

        // Default: forward to configured destination
        self.config.forward_addr
    }

    /// Forward a SIP response: remove proxy's top Via header and forward to
    /// the address indicated by the next Via header.
    async fn forward_response(
        &self,
        mut response: SipResponse,
    ) -> Result<(), SipLoadTestError> {
        // Remove the top Via header (proxy's own Via)
        response.headers.remove_first("Via");

        // Determine destination from the now-top Via header
        let dest = if let Some(via) = response.headers.get("Via") {
            parse_via_addr(via).unwrap_or(self.config.forward_addr)
        } else {
            self.config.forward_addr
        };

        // Extract debug info before formatting consumes the response
        let status_code = response.status_code;
        let call_id_owned = if self.config.debug {
            Some(response.headers.get("Call-ID").unwrap_or("<unknown>").to_string())
        } else {
            None
        };

        if self.config.debug {
            let call_id = call_id_owned.as_deref().unwrap_or("<unknown>");
            eprintln!("{}", format_fwd_res_log(status_code, call_id, dest));
        }

        // Forward the response
        let data = format_message_reuse(&SipMessage::Response(response));
        if let Err(e) = self.transport.send_to(&data, dest).await {
            if self.config.debug {
                let call_id = call_id_owned.as_deref().unwrap_or("<unknown>");
                eprintln!("{}", format_res_error_log(&e.to_string(), status_code, call_id, dest));
            }
            return Err(e);
        }

        Ok(())
    }

    /// Handle a REGISTER request: extract AOR from To header, Contact from Contact header,
    /// store in LocationService, and send 200 OK back to sender.
    async fn handle_register(
        &self,
        request: SipRequest,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        // Extract AOR from To header (e.g., "<sip:alice@example.com>" -> "sip:alice@example.com")
        if let Some(to) = request.headers.get("To") {
            let aor = extract_uri(to);

            // Extract Contact URI if present, otherwise use From URI
            let contact_uri = if let Some(contact) = request.headers.get("Contact") {
                extract_uri(contact)
            } else {
                aor.clone()
            };

            let contact_info = ContactInfo {
                contact_uri,
                address: from,
                expires: Instant::now() + std::time::Duration::from_secs(3600),
            };

            self.location_service.register(&aor, contact_info);
        }

        // Send 200 OK response
        let response = self.build_response(&request, 200, "OK");
        let data = format_message_reuse(&SipMessage::Response(response));
        self.transport.send_to(&data, from).await?;

        Ok(())
    }

    /// Check if a request requires authentication.
    /// Initial INVITE (without Route header) and REGISTER require auth.
    /// In-dialog requests (ACK, BYE, re-INVITE with Route) bypass auth.
    fn requires_auth(&self, request: &SipRequest) -> bool {
        match request.method {
            Method::Invite => {
                // Initial INVITE (no Route header) requires auth
                // re-INVITE with Route header is in-dialog, bypasses auth
                request.headers.get("Route").is_none()
            }
            _ => false, // ACK, BYE, OPTIONS, etc. bypass auth
        }
    }

    /// Check if a request has a Proxy-Authorization header.
    fn has_proxy_auth_header(request: &SipRequest) -> bool {
        request.headers.get("Proxy-Authorization").is_some()
    }

    /// Check if a request has an Authorization header (or Proxy-Authorization).
    fn has_auth_header(request: &SipRequest) -> bool {
        request.headers.get("Authorization").is_some()
            || request.headers.get("Proxy-Authorization").is_some()
    }

    /// Format an AuthChallenge into a Digest challenge header value.
    fn format_challenge(challenge: &crate::auth::AuthChallenge) -> String {
        format!(
            "Digest realm=\"{}\", nonce=\"{}\", algorithm={}",
            challenge.realm, challenge.nonce, challenge.algorithm
        )
    }

    /// Build a SIP response from a request, copying Via, From, To, Call-ID, CSeq headers.
    fn build_response(
        &self,
        request: &SipRequest,
        status_code: u16,
        reason_phrase: &str,
    ) -> SipResponse {
        let mut headers = Headers::new();

        // Copy Via headers (all of them, in order)
        for via in request.headers.get_all("Via") {
            headers.add("Via", via.to_string());
        }

        // Copy From, To, Call-ID, CSeq
        if let Some(from) = request.headers.get("From") {
            headers.set("From", from.to_string());
        }
        if let Some(to) = request.headers.get("To") {
            headers.set("To", to.to_string());
        }
        if let Some(call_id) = request.headers.get("Call-ID") {
            headers.set("Call-ID", call_id.to_string());
        }
        if let Some(cseq) = request.headers.get("CSeq") {
            headers.set("CSeq", cseq.to_string());
        }

        SipResponse {
            version: "SIP/2.0".to_string(),
            status_code,
            reason_phrase: reason_phrase.to_string(),
            headers,
            body: None,
        }
    }
}

/// Parse a SIP URI from a Route or Record-Route header to extract the socket address.
/// Handles formats like: <sip:host:port;lr>, sip:host:port, etc.
fn parse_uri_addr(uri: &str) -> Option<SocketAddr> {
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
fn parse_via_addr(via: &str) -> Option<SocketAddr> {
    let via = via.trim();
    // Skip "SIP/2.0/UDP " prefix
    let after_proto = via.split_whitespace().nth(1)?;
    // Strip parameters
    let host_port = after_proto.split(';').next()?;
    host_port.parse().ok()
}

/// Generate a random branch suffix for Via headers.
fn rand_branch() -> String {
    use rand::Rng;
    let val: u64 = rand::thread_rng().gen();
    format!("{:016x}", val)
}

/// Extract a SIP URI from a header value.
/// Handles formats like: "<sip:alice@example.com>;tag=xxx" -> "sip:alice@example.com"
/// or "sip:alice@example.com" -> "sip:alice@example.com"
fn extract_uri(header_value: &str) -> String {
    let trimmed = header_value.trim();
    if let Some(start) = trimmed.find('<') {
        if let Some(end) = trimmed.find('>') {
            return trimmed[start + 1..end].to_string();
        }
    }
    // No angle brackets: take up to first ';' or space
    trimmed.split(';').next().unwrap_or(trimmed).split_whitespace().next().unwrap_or(trimmed).to_string()
}

/// Method 列挙型を文字列に変換するヘルパー
fn method_to_str(method: &Method) -> &str {
    match method {
        Method::Register => "REGISTER",
        Method::Invite => "INVITE",
        Method::Ack => "ACK",
        Method::Bye => "BYE",
        Method::Options => "OPTIONS",
        Method::Update => "UPDATE",
        Method::Other(s) => s.as_str(),
    }
}

/// デバッグログのイベント種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugEvent {
    RecvReq,
    FwdReq,
    RecvRes,
    FwdRes,
    Error,
}

impl DebugEvent {
    fn label(&self) -> &'static str {
        match self {
            DebugEvent::RecvReq => "RECV_REQ",
            DebugEvent::FwdReq => "FWD_REQ",
            DebugEvent::RecvRes => "RECV_RES",
            DebugEvent::FwdRes => "FWD_RES",
            DebugEvent::Error => "ERROR",
        }
    }
}

/// リクエスト受信ログのフォーマット
fn format_recv_req_log(method: &str, request_uri: &str, from: SocketAddr, call_id: &str) -> String {
    format!(
        "[DEBUG] {} method={} uri={} from={} call-id={}",
        DebugEvent::RecvReq.label(),
        method,
        request_uri,
        from,
        call_id
    )
}

/// リクエスト転送ログのフォーマット
fn format_fwd_req_log(method: &str, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} method={} call-id={} dest={}",
        DebugEvent::FwdReq.label(),
        method,
        call_id,
        dest
    )
}

/// レスポンス受信ログのフォーマット
fn format_recv_res_log(status_code: u16, reason: &str, from: SocketAddr, call_id: &str) -> String {
    format!(
        "[DEBUG] {} status={} reason={} from={} call-id={}",
        DebugEvent::RecvRes.label(),
        status_code,
        reason,
        from,
        call_id
    )
}

/// レスポンス転送ログのフォーマット
fn format_fwd_res_log(status_code: u16, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} status={} call-id={} dest={}",
        DebugEvent::FwdRes.label(),
        status_code,
        call_id,
        dest
    )
}

/// エラーログのフォーマット（リクエスト転送エラー）
fn format_req_error_log(error: &str, method: &str, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} event={} error=\"{}\" method={} call-id={} dest={}",
        DebugEvent::Error.label(),
        DebugEvent::FwdReq.label(),
        error,
        method,
        call_id,
        dest
    )
}

/// エラーログのフォーマット（レスポンス転送エラー）
fn format_res_error_log(error: &str, status_code: u16, call_id: &str, dest: SocketAddr) -> String {
    format!(
        "[DEBUG] {} event={} error=\"{}\" status={} call-id={} dest={}",
        DebugEvent::Error.label(),
        DebugEvent::FwdRes.label(),
        error,
        status_code,
        call_id,
        dest
    )
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
    use crate::sip::parser::parse_sip_message;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    /// Mock transport that records all sent messages for verification.
    struct MockTransport {
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent_messages(&self) -> Vec<(SipMessage, SocketAddr)> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(data, addr)| {
                    parse_sip_message(data).ok().map(|msg| (msg, *addr))
                })
                .collect()
        }

        fn sent_requests(&self) -> Vec<(SipRequest, SocketAddr)> {
            self.sent_messages()
                .into_iter()
                .filter_map(|(msg, addr)| match msg {
                    SipMessage::Request(r) => Some((r, addr)),
                    _ => None,
                })
                .collect()
        }

        fn sent_responses(&self) -> Vec<(SipResponse, SocketAddr)> {
            self.sent_messages()
                .into_iter()
                .filter_map(|(msg, addr)| match msg {
                    SipMessage::Response(r) => Some((r, addr)),
                    _ => None,
                })
                .collect()
        }
    }

    impl SipTransport for MockTransport {
        fn send_to<'a>(
            &'a self,
            data: &'a [u8],
            addr: SocketAddr,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>,
        > {
            self.sent.lock().unwrap().push((data.to_vec(), addr));
            Box::pin(async { Ok(()) })
        }
    }

    fn uac_addr() -> SocketAddr {
        "10.0.0.1:5060".parse().unwrap()
    }

    fn uas_addr() -> SocketAddr {
        "10.0.0.2:5060".parse().unwrap()
    }

    fn proxy_config() -> ProxyConfig {
            ProxyConfig {
                host: "10.0.0.100".to_string(),
                port: 5060,
                forward_addr: uas_addr(),
                domain: "10.0.0.100".to_string(),
                debug: false,
            }
        }

    fn make_proxy(transport: Arc<MockTransport>) -> SipProxy {
        let location_service = Arc::new(LocationService::new());
        SipProxy::new(transport, location_service, None, proxy_config())
    }

    fn make_proxy_with_location_service(
        transport: Arc<MockTransport>,
        location_service: Arc<LocationService>,
    ) -> SipProxy {
        SipProxy::new(transport, location_service, None, proxy_config())
    }

    fn make_invite(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    /// Create an INVITE targeting the proxy's managed domain (10.0.0.100).
    /// Used for tests that need LocationService lookup (local domain routing).
    fn make_invite_local_domain(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", format!("SIP/2.0/UDP {};branch=z9hG4bK776", uac_addr()));
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@10.0.0.100>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Content-Length", "0".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@10.0.0.100".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_register(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc123".to_string());
        headers.set("To", "<sip:alice@example.com>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 REGISTER".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_bye_with_route(call_id: &str, route: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("Route", route.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "2 BYE".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_ack_with_route(call_id: &str, route: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKack1".to_string());
        headers.set("Route", route.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 ACK".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Ack,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_reinvite_with_route(call_id: &str, route: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKre1".to_string());
        headers.set("Route", route.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "2 INVITE".to_string());
        SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_200ok_response(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        // Two Via headers: proxy's Via on top, then UAC's Via
        headers.add("Via", "SIP/2.0/UDP 10.0.0.100:5060;branch=z9hG4bK-proxy-abc".to_string());
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=xyz".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        })
    }

    // ===== Req 19.2: Via header addition on request forwarding =====

    #[tokio::test]
    async fn test_request_forwarding_adds_proxy_via_header() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        // Pre-register bob at local domain so INVITE can be forwarded via LocationService
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-via-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
        let (req, _) = &sent[0];

        // Should have 2 Via headers: proxy's on top, then original
        let vias = req.headers.get_all("Via");
        assert_eq!(vias.len(), 2, "Should have 2 Via headers after proxy adds its own");
        assert!(
            vias[0].contains("10.0.0.100:5060"),
            "Top Via should be proxy's: {}",
            vias[0]
        );
        assert!(
            vias[1].contains("10.0.0.1:5060"),
            "Second Via should be original UAC's: {}",
            vias[1]
        );
    }

    #[tokio::test]
    async fn test_request_forwarding_via_has_branch_parameter() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        // Pre-register bob at local domain so INVITE can be forwarded via LocationService
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-via-branch");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];
        let top_via = req.headers.get_all("Via")[0];
        assert!(
            top_via.contains("branch=z9hG4bK"),
            "Via branch must start with z9hG4bK magic cookie: {}",
            top_via
        );
    }

    #[tokio::test]
    async fn test_request_forwarded_to_default_destination() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        // BYE without Route header uses default forward_addr
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", "call-fwd-1".to_string());
        headers.set("CSeq", "2 BYE".to_string());
        let bye = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        proxy.handle_request(bye, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
        let (_, dest) = &sent[0];
        assert_eq!(*dest, uas_addr(), "Request should be forwarded to default destination");
    }

    // ===== Req 19.3: Via header removal on response forwarding =====

    #[tokio::test]
    async fn test_response_forwarding_removes_proxy_via_header() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-resp-1");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1);
        let (resp, _) = &sent[0];

        // Should have only 1 Via header (proxy's removed)
        let vias = resp.headers.get_all("Via");
        assert_eq!(vias.len(), 1, "Should have 1 Via header after proxy removes its own");
        assert!(
            vias[0].contains("10.0.0.1:5060"),
            "Remaining Via should be UAC's: {}",
            vias[0]
        );
    }

    #[tokio::test]
    async fn test_response_forwarded_to_via_address() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-resp-2");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.1:5060".parse::<SocketAddr>().unwrap(),
            "Response should be forwarded to address from remaining Via header"
        );
    }

    // ===== Req 19.4: Record-Route header insertion for INVITE =====

    #[tokio::test]
    async fn test_invite_forwarding_inserts_record_route() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-rr-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];

        let rr = req.headers.get("Record-Route");
        assert!(rr.is_some(), "INVITE should have Record-Route header");
        let rr_value = rr.unwrap();
        assert!(
            rr_value.contains("10.0.0.100:5060"),
            "Record-Route should contain proxy address: {}",
            rr_value
        );
        assert!(
            rr_value.contains(";lr"),
            "Record-Route should have loose routing parameter: {}",
            rr_value
        );
    }

    #[tokio::test]
    async fn test_register_does_not_get_forwarded() {
        // REGISTER is now handled locally by the proxy (200 OK + LocationService store)
        // It should NOT be forwarded as a request
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let register = make_register("call-rr-2");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "REGISTER should NOT be forwarded");

        // Should get a 200 OK response instead
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, _) = &sent_resps[0];
        assert_eq!(resp.status_code, 200);
    }

    // ===== Req 19.5: Route header-based forwarding for in-dialog requests =====

    #[tokio::test]
    async fn test_bye_with_route_header_forwards_to_route_address() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let bye = make_bye_with_route("call-route-1", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(bye, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "BYE should be forwarded to Route header address"
        );
    }

    #[tokio::test]
    async fn test_ack_with_route_header_forwards_to_route_address() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let ack = make_ack_with_route("call-route-2", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(ack, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "ACK should be forwarded to Route header address"
        );
    }

    #[tokio::test]
    async fn test_reinvite_with_route_header_forwards_to_route_address() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let reinvite = make_reinvite_with_route("call-route-3", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(reinvite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "re-INVITE should be forwarded to Route header address"
        );
    }

    #[tokio::test]
    async fn test_reinvite_with_route_also_adds_record_route() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let reinvite = make_reinvite_with_route("call-route-rr", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(reinvite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];
        assert!(
            req.headers.get("Record-Route").is_some(),
            "re-INVITE should also have Record-Route header"
        );
    }

    // ===== Req 19.9: Stateless operation =====

    #[tokio::test]
    async fn test_proxy_is_stateless_no_dialog_tracking() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        // Send multiple independent requests - proxy should handle each independently
        let invite1 = make_invite_local_domain("call-stateless-1");
        let invite2 = make_invite_local_domain("call-stateless-2");

        proxy.handle_request(invite1, uac_addr()).await.unwrap();
        proxy.handle_request(invite2, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 2, "Both requests should be forwarded independently");
    }

    #[tokio::test]
    async fn test_proxy_handles_bye_without_prior_invite() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        // BYE without Route header and without prior INVITE - should still forward
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", "call-no-state".to_string());
        headers.set("CSeq", "2 BYE".to_string());
        let bye = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        let result = proxy.handle_request(bye, uac_addr()).await;
        assert!(result.is_ok(), "Stateless proxy should forward BYE without prior state");

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
    }

    #[tokio::test]
    async fn test_response_message_ignored_by_handle_request() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-ignore");
        let result = proxy.handle_request(response, uas_addr()).await;
        assert!(result.is_ok());
        assert!(transport.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_request_message_ignored_by_handle_response() {
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let invite = make_invite("call-ignore-2");
        let result = proxy.handle_response(invite, uac_addr()).await;
        assert!(result.is_ok());
        assert!(transport.sent.lock().unwrap().is_empty());
    }

    // ===== Via header preservation tests =====

    #[tokio::test]
    async fn test_original_headers_preserved_after_forwarding() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-preserve");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];

        // Original headers should be preserved
        assert_eq!(req.headers.get("Call-ID"), Some("call-preserve"));
        assert_eq!(req.headers.get("CSeq"), Some("1 INVITE"));
        assert!(req.headers.get("From").is_some());
        assert!(req.headers.get("To").is_some());
    }

    // ===== Helper function tests =====

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

    #[test]
    fn test_parse_via_addr_standard() {
        let addr = parse_via_addr("SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776");
        assert_eq!(addr, Some("10.0.0.1:5060".parse().unwrap()));
    }

    #[test]
    fn test_parse_via_addr_invalid() {
        assert!(parse_via_addr("invalid").is_none());
    }

    // ===== is_local_domain tests =====

    #[test]
    fn test_is_local_domain_with_user_and_matching_domain() {
        let proxy = make_proxy(Arc::new(MockTransport::new()));
        assert!(proxy.is_local_domain("sip:bob@10.0.0.100"));
    }

    #[test]
    fn test_is_local_domain_with_user_and_non_matching_domain() {
        let proxy = make_proxy(Arc::new(MockTransport::new()));
        assert!(!proxy.is_local_domain("sip:bob@example.com"));
    }

    #[test]
    fn test_is_local_domain_with_port_stripped() {
        let proxy = make_proxy(Arc::new(MockTransport::new()));
        assert!(proxy.is_local_domain("sip:bob@10.0.0.100:5060"));
    }

    #[test]
    fn test_is_local_domain_without_user_part() {
        let proxy = make_proxy(Arc::new(MockTransport::new()));
        assert!(proxy.is_local_domain("sip:10.0.0.100"));
    }

    #[test]
    fn test_is_local_domain_without_sip_prefix() {
        let proxy = make_proxy(Arc::new(MockTransport::new()));
        assert!(proxy.is_local_domain("bob@10.0.0.100"));
    }

    // ===== LocationService unit tests =====

    #[test]
    fn test_location_service_register_and_lookup() {
        let ls = LocationService::new();
        let contact = ContactInfo {
            contact_uri: "sip:alice@10.0.0.1:5060".to_string(),
            address: "10.0.0.1:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact);

        let result = ls.lookup("sip:alice@example.com");
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.contact_uri, "sip:alice@10.0.0.1:5060");
        assert_eq!(info.address, "10.0.0.1:5060".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn test_location_service_lookup_unregistered_returns_none() {
        let ls = LocationService::new();
        assert!(ls.lookup("sip:unknown@example.com").is_none());
    }

    #[test]
    fn test_location_service_register_overwrites_previous() {
        let ls = LocationService::new();
        let contact1 = ContactInfo {
            contact_uri: "sip:alice@10.0.0.1:5060".to_string(),
            address: "10.0.0.1:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact1);

        let contact2 = ContactInfo {
            contact_uri: "sip:alice@10.0.0.2:5060".to_string(),
            address: "10.0.0.2:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        ls.register("sip:alice@example.com", contact2);

        let result = ls.lookup("sip:alice@example.com").unwrap();
        assert_eq!(result.contact_uri, "sip:alice@10.0.0.2:5060");
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

    // ===== Req 19.6: REGISTER stores in LocationService and returns 200 OK =====

    #[tokio::test]
    async fn test_register_stores_in_location_service_and_returns_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc123".to_string());
        headers.set("To", "<sip:alice@example.com>".to_string());
        headers.set("Call-ID", "reg-loc-1".to_string());
        headers.set("CSeq", "1 REGISTER".to_string());
        headers.set("Contact", "<sip:alice@10.0.0.1:5060>".to_string());
        let register = SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        proxy.handle_request(register, uac_addr()).await.unwrap();

        // Verify 200 OK was sent back
        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1);
        let (resp, _) = &sent[0];
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");

        // Verify registration was stored in LocationService
        let contact = location_service.lookup("sip:alice@example.com");
        assert!(contact.is_some(), "Registration should be stored in LocationService");
        let info = contact.unwrap();
        assert_eq!(info.contact_uri, "sip:alice@10.0.0.1:5060");
        assert_eq!(info.address, uac_addr());
    }

    #[tokio::test]
    async fn test_register_200_ok_copies_headers() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service.clone());

        let register = make_register("reg-headers-1");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        let sent = transport.sent_responses();
        let (resp, _) = &sent[0];
        assert_eq!(resp.headers.get("Call-ID"), Some("reg-headers-1"));
        assert!(resp.headers.get("From").is_some());
        assert!(resp.headers.get("To").is_some());
        assert!(resp.headers.get("CSeq").is_some());
    }

    #[tokio::test]
    async fn test_register_200_ok_sent_to_sender() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service.clone());

        let register = make_register("reg-dest-1");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        let sent = transport.sent_responses();
        let (_, dest) = &sent[0];
        assert_eq!(*dest, uac_addr(), "200 OK should be sent back to the sender");
    }

    #[tokio::test]
    async fn test_register_is_not_forwarded_to_uas() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service.clone());

        let register = make_register("reg-nofwd-1");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        // Should only have a response (200 OK), no forwarded request
        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "REGISTER should NOT be forwarded to UAS");
    }

    // ===== Req 19.7: INVITE with registered URI forwards to registered Contact =====

    #[tokio::test]
    async fn test_invite_with_registered_uri_forwards_to_contact_address() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());

        // Pre-register bob at local domain
        let contact = ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: "10.0.0.2:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        location_service.register("sip:bob@10.0.0.100", contact);

        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-loc-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "INVITE should be forwarded to registered Contact address"
        );
    }

    #[tokio::test]
    async fn test_invite_with_registered_uri_has_via_and_record_route() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());

        let contact = ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: "10.0.0.2:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        location_service.register("sip:bob@10.0.0.100", contact);

        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-loc-2");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];

        // Should have proxy Via header added
        let vias = req.headers.get_all("Via");
        assert!(vias.len() >= 2, "Should have proxy Via header added");
        assert!(vias[0].contains("10.0.0.100:5060"), "Top Via should be proxy's");

        // Should have Record-Route
        let rr = req.headers.get("Record-Route");
        assert!(rr.is_some(), "INVITE should have Record-Route header");
    }

    // ===== Req 19.8: INVITE with unregistered URI returns 404 Not Found =====

    #[tokio::test]
    async fn test_invite_with_unregistered_uri_forwards_to_default() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        // No registrations in LocationService

        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite("call-fwd-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should be forwarded to forward_addr, NOT get a 404
        let sent_resps = transport.sent_responses();
        assert!(sent_resps.is_empty(), "Unregistered INVITE should NOT get a 404 response");

        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "Unregistered INVITE should be forwarded");
        let (_req, dest) = &sent_reqs[0];
        assert_eq!(*dest, uas_addr(), "Should be forwarded to default forward_addr");
    }

    #[tokio::test]
    async fn test_invite_unregistered_uri_forwarded_has_via_and_record_route() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite("call-fwd-headers");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1);
        let (req, _) = &sent_reqs[0];

        // Via header should be added by proxy
        let vias = req.headers.get_all("Via");
        assert!(vias.len() >= 2, "Should have proxy Via + original Via");
        assert!(vias[0].contains("10.0.0.100:5060"), "First Via should be proxy's");

        // Record-Route should be added for INVITE
        let rr = req.headers.get("Record-Route");
        assert!(rr.is_some(), "INVITE should have Record-Route");
        assert!(rr.unwrap().contains("10.0.0.100:5060"), "Record-Route should contain proxy addr");
        assert!(rr.unwrap().contains(";lr"), "Record-Route should have ;lr parameter");
    }

    #[tokio::test]
    async fn test_invite_targeting_proxy_itself_returns_404() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        // Build an INVITE with Request-URI targeting the proxy itself
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@10.0.0.100:5060>".to_string());
        headers.set("Call-ID", "call-loop-detect".to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        let invite = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@10.0.0.100:5060".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should get a 404 response (loop prevention)
        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "Request targeting proxy itself should NOT be forwarded");

        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, dest) = &sent_resps[0];
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason_phrase, "Not Found");
        assert_eq!(*dest, uac_addr(), "404 should be sent back to the sender");
    }

    // ===== Non-local domain INVITE forwarding =====

    #[tokio::test]
    async fn test_invite_non_local_domain_forwarded_to_forward_addr() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        // make_invite creates sip:bob@example.com which is NOT the proxy's domain (10.0.0.100)
        let invite = make_invite("call-nonlocal-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should be forwarded as a request, NOT a 404 response
        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "Non-local INVITE should be forwarded");
        let (_, dest) = &sent_reqs[0];
        assert_eq!(*dest, uas_addr(), "Should be forwarded to forward_addr");

        // No error responses should be sent
        let sent_resps = transport.sent_responses();
        assert!(sent_resps.is_empty(), "No error response for non-local domain");
    }

    #[tokio::test]
    async fn test_invite_local_domain_unregistered_returns_404() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        // No registrations
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        // INVITE targeting the proxy's managed domain, but user not registered
        let invite = make_invite_local_domain("call-local-unreg");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should get 404 because it's a local domain and user is not registered
        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "Unregistered local domain INVITE should NOT be forwarded");

        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, dest) = &sent_resps[0];
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason_phrase, "Not Found");
        assert_eq!(*dest, uac_addr());
    }

    #[tokio::test]
    async fn test_invite_local_domain_registered_forwards_to_contact() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());

        // Register bob at the proxy's managed domain
        let contact = ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: "10.0.0.2:5060".parse().unwrap(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        };
        location_service.register("sip:bob@10.0.0.100", contact);

        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-local-reg");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1);
        let (_, dest) = &sent_reqs[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "Local domain registered INVITE should forward to contact"
        );
    }

    // ===== In-dialog requests still use Route-based forwarding =====

    #[tokio::test]
    async fn test_bye_with_route_still_works_with_location_service() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let bye = make_bye_with_route("call-route-ls", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(bye, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
        let (_, dest) = &sent[0];
        assert_eq!(
            *dest,
            "10.0.0.2:5060".parse::<SocketAddr>().unwrap(),
            "BYE with Route should still use Route-based forwarding"
        );
    }

    #[tokio::test]
    async fn test_reinvite_with_route_uses_route_not_location_service() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        // Don't register bob - but re-INVITE has Route header so should still forward
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let reinvite = make_reinvite_with_route("call-route-reinv", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(reinvite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "re-INVITE with Route should be forwarded, not 404'd");
    }

    // ===== Auth integration helpers =====

    fn make_user_pool() -> Arc<crate::user_pool::UserPool> {
        use crate::user_pool::{UsersFile, UserEntry, UserPool};
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

    fn make_proxy_with_auth(
        transport: Arc<MockTransport>,
        location_service: Arc<LocationService>,
    ) -> SipProxy {
        use crate::auth::ProxyAuth;
        let pool = make_user_pool();
        let auth = ProxyAuth::new("example.com".to_string(), pool);
        SipProxy::new(transport, location_service, Some(auth), proxy_config())
    }

    fn make_invite_with_proxy_auth(call_id: &str, auth_header: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "2 INVITE".to_string());
        headers.add("Proxy-Authorization", auth_header.to_string());
        SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_register_with_auth(call_id: &str, auth_header: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc123".to_string());
        headers.set("To", "<sip:alice@example.com>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "2 REGISTER".to_string());
        headers.set("Contact", "<sip:alice@10.0.0.1:5060>".to_string());
        headers.add("Authorization", auth_header.to_string());
        SipMessage::Request(SipRequest {
            method: Method::Register,
            request_uri: "sip:registrar.example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn compute_valid_digest(
        username: &str,
        password: &str,
        realm: &str,
        nonce: &str,
        method: &str,
        uri: &str,
    ) -> String {
        use crate::auth::{AuthChallenge, DigestAuth};
        let challenge = AuthChallenge {
            realm: realm.to_string(),
            nonce: nonce.to_string(),
            algorithm: "MD5".to_string(),
        };
        DigestAuth::compute_response(username, password, &challenge, method, uri)
    }

    // ===== Req 20.6: INVITE without auth → 407 Proxy Authentication Required =====

    #[tokio::test]
    async fn test_invite_without_auth_returns_407_when_auth_enabled() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@example.com", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let invite = make_invite("call-auth-407");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should NOT forward the request
        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "INVITE without auth should NOT be forwarded");

        // Should send 407 response
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, dest) = &sent_resps[0];
        assert_eq!(resp.status_code, 407);
        assert_eq!(resp.reason_phrase, "Proxy Authentication Required");
        assert_eq!(*dest, uac_addr());
    }

    #[tokio::test]
    async fn test_407_response_contains_proxy_authenticate_header() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let invite = make_invite("call-auth-407-hdr");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent_resps = transport.sent_responses();
        let (resp, _) = &sent_resps[0];
        let pa = resp.headers.get("Proxy-Authenticate");
        assert!(pa.is_some(), "407 should contain Proxy-Authenticate header");
        let pa_value = pa.unwrap();
        assert!(pa_value.contains("realm="), "Proxy-Authenticate should contain realm");
        assert!(pa_value.contains("nonce="), "Proxy-Authenticate should contain nonce");
        assert!(pa_value.contains("algorithm="), "Proxy-Authenticate should contain algorithm");
    }

    #[tokio::test]
    async fn test_407_response_copies_request_headers() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let invite = make_invite("call-auth-407-copy");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent_resps = transport.sent_responses();
        let (resp, _) = &sent_resps[0];
        assert_eq!(resp.headers.get("Call-ID"), Some("call-auth-407-copy"));
        assert!(resp.headers.get("Via").is_some());
        assert!(resp.headers.get("From").is_some());
        assert!(resp.headers.get("To").is_some());
        assert!(resp.headers.get("CSeq").is_some());
    }

    // ===== Req 20.7: REGISTER without auth → 401 Unauthorized =====

    #[tokio::test]
    async fn test_register_without_auth_returns_401_when_auth_enabled() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let register = make_register("call-auth-401");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        // Should NOT store in location service or send 200 OK
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, dest) = &sent_resps[0];
        assert_eq!(resp.status_code, 401);
        assert_eq!(resp.reason_phrase, "Unauthorized");
        assert_eq!(*dest, uac_addr());
    }

    #[tokio::test]
    async fn test_401_response_contains_www_authenticate_header() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let register = make_register("call-auth-401-hdr");
        proxy.handle_request(register, uac_addr()).await.unwrap();

        let sent_resps = transport.sent_responses();
        let (resp, _) = &sent_resps[0];
        let wa = resp.headers.get("WWW-Authenticate");
        assert!(wa.is_some(), "401 should contain WWW-Authenticate header");
        let wa_value = wa.unwrap();
        assert!(wa_value.contains("realm="), "WWW-Authenticate should contain realm");
        assert!(wa_value.contains("nonce="), "WWW-Authenticate should contain nonce");
        assert!(wa_value.contains("algorithm="), "WWW-Authenticate should contain algorithm");
    }

    // ===== Req 20.8: Valid auth → request processed =====

    #[tokio::test]
    async fn test_invite_with_valid_auth_is_forwarded() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@example.com", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        // We need to use the same realm as the proxy ("example.com")
        // and a nonce. Since we can't predict the nonce from create_challenge,
        // we'll construct the auth header with a known nonce and the proxy
        // will verify using the same realm.
        // Actually, the proxy's verify() just checks the digest computation
        // against the UserPool, using the nonce from the auth header itself.
        let nonce = "abc123def456";
        let digest = compute_valid_digest(
            "alice", "secret123", "example.com", nonce, "INVITE", "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            nonce, digest
        );

        let invite = make_invite_with_proxy_auth("call-auth-ok", &auth_header);
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should be forwarded (no 407/403)
        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "Valid auth INVITE should be forwarded");
        let sent_resps = transport.sent_responses();
        assert!(sent_resps.is_empty(), "No error response should be sent for valid auth");
    }

    #[tokio::test]
    async fn test_register_with_valid_auth_stores_and_returns_200() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service.clone());

        let nonce = "reg_nonce_123";
        let digest = compute_valid_digest(
            "alice", "secret123", "example.com", nonce, "REGISTER", "sip:registrar.example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:registrar.example.com\", response=\"{}\", algorithm=MD5",
            nonce, digest
        );

        let register = make_register_with_auth("call-auth-reg-ok", &auth_header);
        proxy.handle_request(register, uac_addr()).await.unwrap();

        // Should get 200 OK
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, _) = &sent_resps[0];
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");

        // Should be stored in location service
        let contact = location_service.lookup("sip:alice@example.com");
        assert!(contact.is_some(), "Valid auth REGISTER should store in LocationService");
    }

    // ===== Req 20.9: Invalid auth → 403 Forbidden =====

    #[tokio::test]
    async fn test_invite_with_invalid_auth_returns_403() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@example.com", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        // Use wrong password to compute digest
        let nonce = "bad_nonce_123";
        let digest = compute_valid_digest(
            "alice", "wrongpassword", "example.com", nonce, "INVITE", "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            nonce, digest
        );

        let invite = make_invite_with_proxy_auth("call-auth-403", &auth_header);
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should NOT forward
        let sent_reqs = transport.sent_requests();
        assert!(sent_reqs.is_empty(), "Invalid auth INVITE should NOT be forwarded");

        // Should send 403
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, dest) = &sent_resps[0];
        assert_eq!(resp.status_code, 403);
        assert_eq!(resp.reason_phrase, "Forbidden");
        assert_eq!(*dest, uac_addr());
    }

    #[tokio::test]
    async fn test_register_with_invalid_auth_returns_403() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service.clone());

        let nonce = "bad_reg_nonce";
        let digest = compute_valid_digest(
            "alice", "wrongpassword", "example.com", nonce, "REGISTER", "sip:registrar.example.com",
        );
        let auth_header = format!(
            "Digest username=\"alice\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:registrar.example.com\", response=\"{}\", algorithm=MD5",
            nonce, digest
        );

        let register = make_register_with_auth("call-auth-reg-403", &auth_header);
        proxy.handle_request(register, uac_addr()).await.unwrap();

        // Should send 403
        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, _) = &sent_resps[0];
        assert_eq!(resp.status_code, 403);
        assert_eq!(resp.reason_phrase, "Forbidden");

        // Should NOT store in location service
        let contact = location_service.lookup("sip:alice@example.com");
        assert!(contact.is_none(), "Invalid auth REGISTER should NOT store in LocationService");
    }

    #[tokio::test]
    async fn test_invite_with_unknown_user_returns_403() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@example.com", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let nonce = "unknown_nonce";
        let digest = compute_valid_digest(
            "unknown_user", "somepass", "example.com", nonce, "INVITE", "sip:bob@example.com",
        );
        let auth_header = format!(
            "Digest username=\"unknown_user\", realm=\"example.com\", nonce=\"{}\", uri=\"sip:bob@example.com\", response=\"{}\", algorithm=MD5",
            nonce, digest
        );

        let invite = make_invite_with_proxy_auth("call-auth-unknown", &auth_header);
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent_resps = transport.sent_responses();
        assert_eq!(sent_resps.len(), 1);
        let (resp, _) = &sent_resps[0];
        assert_eq!(resp.status_code, 403);
    }

    // ===== Auth disabled: no challenge issued =====

    #[tokio::test]
    async fn test_invite_without_auth_passes_when_auth_disabled() {
        // This is the existing behavior: auth=None means no auth check
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@example.com", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite("call-no-auth");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        // Should be forwarded without auth challenge
        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "INVITE should be forwarded when auth is disabled");
    }

    // ===== In-dialog requests (ACK, BYE) bypass auth =====

    #[tokio::test]
    async fn test_ack_bypasses_auth_check() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let ack = make_ack_with_route("call-ack-noauth", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(ack, uac_addr()).await.unwrap();

        // ACK should be forwarded without auth
        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "ACK should bypass auth and be forwarded");
    }

    #[tokio::test]
    async fn test_bye_bypasses_auth_check() {
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        let proxy = make_proxy_with_auth(transport.clone(), location_service);

        let bye = make_bye_with_route("call-bye-noauth", "<sip:10.0.0.2:5060;lr>");
        proxy.handle_request(bye, uac_addr()).await.unwrap();

        // BYE should be forwarded without auth
        let sent_reqs = transport.sent_requests();
        assert_eq!(sent_reqs.len(), 1, "BYE should bypass auth and be forwarded");
    }

    // ===== DebugEvent label テスト =====

    #[test]
    fn test_debug_event_labels() {
        assert_eq!(DebugEvent::RecvReq.label(), "RECV_REQ");
        assert_eq!(DebugEvent::FwdReq.label(), "FWD_REQ");
        assert_eq!(DebugEvent::RecvRes.label(), "RECV_RES");
        assert_eq!(DebugEvent::FwdRes.label(), "FWD_RES");
        assert_eq!(DebugEvent::Error.label(), "ERROR");
    }

    // ===== フォーマット関数のユニットテスト =====
    // Requirements: 2.1, 2.2, 3.1, 3.2, 4.1, 4.2, 5.1, 5.2, 6.1, 6.2, 7.1, 7.2

    #[test]
    fn test_format_recv_req_log() {
        let from: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_recv_req_log("INVITE", "sip:bob@example.com", from, "abc123@host");
        assert_eq!(
            result,
            "[DEBUG] RECV_REQ method=INVITE uri=sip:bob@example.com from=192.168.1.1:5060 call-id=abc123@host"
        );
    }

    #[test]
    fn test_format_fwd_req_log() {
        let dest: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_fwd_req_log("INVITE", "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] FWD_REQ method=INVITE call-id=abc123@host dest=10.0.0.1:5080"
        );
    }

    #[test]
    fn test_format_recv_res_log() {
        let from: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_recv_res_log(200, "OK", from, "abc123@host");
        assert_eq!(
            result,
            "[DEBUG] RECV_RES status=200 reason=OK from=10.0.0.1:5080 call-id=abc123@host"
        );
    }

    #[test]
    fn test_format_fwd_res_log() {
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_fwd_res_log(200, "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] FWD_RES status=200 call-id=abc123@host dest=192.168.1.1:5060"
        );
    }

    #[test]
    fn test_format_req_error_log() {
        let dest: SocketAddr = "10.0.0.1:5080".parse().unwrap();
        let result = format_req_error_log("connection refused", "INVITE", "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] ERROR event=FWD_REQ error=\"connection refused\" method=INVITE call-id=abc123@host dest=10.0.0.1:5080"
        );
    }

    #[test]
    fn test_format_res_error_log() {
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let result = format_res_error_log("connection refused", 200, "abc123@host", dest);
        assert_eq!(
            result,
            "[DEBUG] ERROR event=FWD_RES error=\"connection refused\" status=200 call-id=abc123@host dest=192.168.1.1:5060"
        );
    }

    // ===== method_to_str ヘルパー関数テスト =====

    #[test]
    fn test_method_to_str_known_methods() {
        assert_eq!(method_to_str(&Method::Register), "REGISTER");
        assert_eq!(method_to_str(&Method::Invite), "INVITE");
        assert_eq!(method_to_str(&Method::Ack), "ACK");
        assert_eq!(method_to_str(&Method::Bye), "BYE");
        assert_eq!(method_to_str(&Method::Options), "OPTIONS");
        assert_eq!(method_to_str(&Method::Update), "UPDATE");
    }

    #[test]
    fn test_method_to_str_other() {
        assert_eq!(method_to_str(&Method::Other("SUBSCRIBE".to_string())), "SUBSCRIBE");
    }

    // ===== handle_request デバッグログ統合テスト =====
    // Requirements: 1.3, 1.4, 2.1, 2.2, 7.3

    fn make_debug_proxy_config() -> ProxyConfig {
        ProxyConfig {
            host: "10.0.0.100".to_string(),
            port: 5060,
            forward_addr: uas_addr(),
            domain: "10.0.0.100".to_string(),
            debug: true,
        }
    }

    fn make_debug_proxy(transport: Arc<MockTransport>) -> SipProxy {
        let location_service = Arc::new(LocationService::new());
        SipProxy::new(transport, location_service, None, make_debug_proxy_config())
    }

    #[tokio::test]
    async fn test_handle_request_with_debug_enabled_still_forwards() {
        // debug=true でもリクエスト転送が正常に動作することを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let invite = make_invite("call-debug-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "debug=true でもリクエストは転送されるべき");
    }

    #[tokio::test]
    async fn test_handle_request_debug_with_missing_call_id_still_works() {
        // Call-ID ヘッダがないリクエストでも debug ログが "<unknown>" を使用してパニックしないことを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
        // Call-ID を意図的に設定しない
        headers.set("CSeq", "1 INVITE".to_string());
        let msg = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        // パニックせずに正常に処理されることを確認
        proxy.handle_request(msg, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "Call-ID がなくてもリクエストは転送されるべき");
    }

    #[tokio::test]
    async fn test_handle_request_no_debug_log_when_disabled() {
        // debug=false の場合、通常通り動作する（ログ出力なし）
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let invite = make_invite("call-no-debug");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "debug=false でもリクエストは転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_request_with_debug_enabled_still_forwards() {
        // debug=true でも forward_request によるリクエスト転送が正常に動作することを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let invite = make_invite("call-fwd-debug-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "debug=true でもリクエストは転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_request_debug_with_missing_call_id_still_works() {
        // Call-ID ヘッダがないリクエストでも forward_request の debug ログが "<unknown>" を使用してパニックしないことを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
        // Call-ID を意図的に設定しない
        headers.set("CSeq", "1 BYE".to_string());
        headers.set("Route", "<sip:10.0.0.100:5060;lr>".to_string());
        let msg = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        // パニックせずに正常に処理されることを確認
        proxy.handle_request(msg, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "Call-ID がなくても forward_request は転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_request_no_debug_log_when_disabled() {
        // debug=false の場合、forward_request も通常通り動作する
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let invite = make_invite("call-fwd-no-debug");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1, "debug=false でもリクエストは転送されるべき");
    }

    #[tokio::test]
    async fn test_handle_response_with_debug_enabled_still_forwards() {
        // debug=true でもレスポンス転送が正常に動作することを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let response = make_200ok_response("call-res-debug-1");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "debug=true でもレスポンスは転送されるべき");
    }

    #[tokio::test]
    async fn test_handle_response_debug_with_missing_call_id_still_works() {
        // Call-ID ヘッダがないレスポンスでも debug=true で正常動作することを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.100:5060;branch=z9hG4bK-proxy-abc".to_string());
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=xyz".to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "Call-ID がなくてもレスポンスは転送されるべき");
    }

    #[tokio::test]
    async fn test_handle_response_no_debug_log_when_disabled() {
        // debug=false の場合、handle_response も通常通り動作する
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-res-no-debug");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "debug=false でもレスポンスは転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_response_with_debug_enabled_still_forwards() {
        // debug=true でも forward_response によるレスポンス転送が正常に動作することを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let response = make_200ok_response("call-fwd-res-debug-1");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "debug=true でもレスポンスは転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_response_debug_with_missing_call_id_still_works() {
        // Call-ID ヘッダがないレスポンスでも forward_response の debug ログが "<unknown>" を使用してパニックしないことを確認
        let transport = Arc::new(MockTransport::new());
        let proxy = make_debug_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.100:5060;branch=z9hG4bK-proxy-abc".to_string());
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=xyz".to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        // Call-ID を意図的に設定しない
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        // パニックせずに正常に処理されることを確認
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "Call-ID がなくても forward_response は転送されるべき");
    }

    #[tokio::test]
    async fn test_forward_response_no_debug_log_when_disabled() {
        // debug=false の場合、forward_response も通常通り動作する
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-fwd-res-no-debug");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1, "debug=false でもレスポンスは転送されるべき");
    }



    // ===== Task 12.1: プロキシのバッファ再利用・高速RNG テスト =====

    #[test]
    fn test_rand_branch_returns_16_char_hex_string() {
        // rand_branch() は u64 ベースの16文字16進数文字列を返すべき
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
        // 連続生成で一意な値を返すべき（SystemTimeベースでは衝突リスクあり）
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

    #[tokio::test]
    async fn test_forward_request_uses_format_into_compatible_output() {
        // format_into + thread-local buffer を使用しても、出力は format_sip_message と同一であるべき
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=abc".to_string());
        headers.set("Call-ID", "call-fmt-into-1".to_string());
        headers.set("CSeq", "2 BYE".to_string());
        let bye = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        proxy.handle_request(bye, uac_addr()).await.unwrap();

        // transport に送信されたバイト列がパース可能であることを確認
        let sent = transport.sent_requests();
        assert_eq!(sent.len(), 1);
        let (req, _) = &sent[0];
        // Via ヘッダが正しく追加されていること
        let vias = req.headers.get_all("Via");
        assert!(vias.len() >= 2, "Should have proxy Via + original Via");
        assert!(vias[0].contains("10.0.0.100:5060"), "Top Via should be proxy's");
    }

    #[tokio::test]
    async fn test_forward_response_uses_format_into_compatible_output() {
        // レスポンス転送でも format_into + thread-local buffer の出力がパース可能であるべき
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let response = make_200ok_response("call-fmt-into-resp-1");
        proxy.handle_response(response, uas_addr()).await.unwrap();

        let sent = transport.sent_responses();
        assert_eq!(sent.len(), 1);
        let (resp, _) = &sent[0];
        // プロキシの Via が除去されていること
        let vias = resp.headers.get_all("Via");
        assert_eq!(vias.len(), 1, "Proxy Via should be removed, leaving 1 Via");
    }

    #[tokio::test]
    async fn test_via_header_format_is_correct_after_optimization() {
        // Via ヘッダの形式が "SIP/2.0/UDP host:port;branch=z9hG4bK-proxy-XXXXXXXXXXXXXXXX" であるべき
        let transport = Arc::new(MockTransport::new());
        let proxy = make_proxy(transport.clone());

        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=t1".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=t2".to_string());
        headers.set("Call-ID", "call-via-fmt-1".to_string());
        headers.set("CSeq", "2 BYE".to_string());
        let bye = SipMessage::Request(SipRequest {
            method: Method::Bye,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        });

        proxy.handle_request(bye, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];
        let top_via = req.headers.get_all("Via")[0];

        // Via の形式を検証
        assert!(
            top_via.starts_with("SIP/2.0/UDP 10.0.0.100:5060;branch=z9hG4bK-proxy-"),
            "Via should have correct format, got: '{}'",
            top_via
        );

        // branch suffix は16文字の16進数であるべき
        let branch_prefix = "z9hG4bK-proxy-";
        let branch_start = top_via.find(branch_prefix).unwrap() + branch_prefix.len();
        let branch_suffix = &top_via[branch_start..];
        assert_eq!(
            branch_suffix.len(),
            16,
            "Branch suffix should be 16 hex chars, got: '{}'",
            branch_suffix
        );
        assert!(
            branch_suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "Branch suffix should be hex, got: '{}'",
            branch_suffix
        );
    }

    #[tokio::test]
    async fn test_record_route_format_is_correct_after_optimization() {
        // Record-Route ヘッダの形式が "<sip:host:port;lr>" であるべき
        let transport = Arc::new(MockTransport::new());
        let location_service = Arc::new(LocationService::new());
        location_service.register("sip:bob@10.0.0.100", ContactInfo {
            contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
            address: uas_addr(),
            expires: Instant::now() + std::time::Duration::from_secs(3600),
        });
        let proxy = make_proxy_with_location_service(transport.clone(), location_service);

        let invite = make_invite_local_domain("call-rr-fmt-1");
        proxy.handle_request(invite, uac_addr()).await.unwrap();

        let sent = transport.sent_requests();
        let (req, _) = &sent[0];
        let rr = req.headers.get("Record-Route").unwrap();
        assert_eq!(
            rr,
            "<sip:10.0.0.100:5060;lr>",
            "Record-Route should have exact format"
        );
    }

    // ===== Property 5: プロキシリクエスト転送のVia/Record-Route正確性テスト =====
    // Feature: performance-bottleneck-optimization, Property 5
    // **Validates: Requirements 5.1**

    use proptest::prelude::*;

    /// Strategy for generating an arbitrary SIP INVITE request with non-local domain
    /// (so the proxy forwards to default forward_addr without LocationService lookup).
    fn arb_invite_request_non_local() -> impl Strategy<Value = (SipRequest, Vec<String>)> {
        (
            // Generate 1..4 original Via headers
            proptest::collection::vec(
                (
                    1u8..=254,
                    1u8..=254,
                    1u8..=254,
                    1u8..=254,
                    1024u16..60000,
                    "[a-zA-Z0-9]{4,12}",
                )
                    .prop_map(|(a, b, c, d, port, branch)| {
                        format!(
                            "SIP/2.0/UDP {}.{}.{}.{}:{};branch=z9hG4bK{}",
                            a, b, c, d, port, branch
                        )
                    }),
                1..4,
            ),
            "[a-zA-Z0-9]{1,32}",  // call_id
            "[a-zA-Z0-9]{1,16}",  // from_tag
            // Non-local domain request URI (not matching proxy host 10.0.0.100)
            (
                "[a-z][a-z0-9]{0,7}",
                "[a-z][a-z0-9]{0,7}\\.[a-z]{2,4}",
            )
                .prop_map(|(user, domain)| format!("sip:{}@{}", user, domain)),
            // 0..4 extra non-Via/non-Record-Route headers
            proptest::collection::vec(
                (
                    "[A-Z][a-z]{2,10}",
                    "[a-zA-Z0-9 ]{1,20}",
                )
                    .prop_filter("avoid reserved header names", |(name, _)| {
                        let lower = name.to_ascii_lowercase();
                        lower != "via" && lower != "record-route" && lower != "route"
                            && lower != "from" && lower != "to" && lower != "call-id"
                            && lower != "cseq" && lower != "content-length"
                    }),
                0..4,
            ),
        )
            .prop_map(|(vias, call_id, from_tag, request_uri, extra_headers)| {
                let mut headers = Headers::new();
                for via in &vias {
                    headers.add("Via", via.clone());
                }
                headers.set("From", format!("<sip:alice@example.com>;tag={}", from_tag));
                headers.set("To", format!("<{}>", request_uri));
                headers.set("Call-ID", call_id);
                headers.set("CSeq", "1 INVITE".to_string());
                for (name, value) in &extra_headers {
                    headers.add(name, value.clone());
                }

                let original_vias = vias.clone();
                let req = SipRequest {
                    method: Method::Invite,
                    request_uri,
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                };
                (req, original_vias)
            })
    }

    proptest! {
        /// Feature: performance-bottleneck-optimization, Property 5
        /// 任意のSIP INVITEリクエストに対して、転送後にプロキシのViaが最上位に追加され、
        /// Record-Routeが含まれる
        /// **Validates: Requirements 5.1**
        #[test]
        fn prop_proxy_via_record_route(
            (request, original_vias) in arb_invite_request_non_local(),
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                let proxy = make_proxy(transport.clone());

                proxy
                    .handle_request(SipMessage::Request(request.clone()), uac_addr())
                    .await
                    .unwrap();

                let sent_reqs = transport.sent_requests();
                prop_assert_eq!(
                    sent_reqs.len(), 1,
                    "Proxy should forward exactly one request"
                );
                let (forwarded_req, _dest) = &sent_reqs[0];

                // 1. Proxy's Via is added as the topmost Via
                let forwarded_vias = forwarded_req.headers.get_all("Via");
                prop_assert_eq!(
                    forwarded_vias.len(),
                    original_vias.len() + 1,
                    "Forwarded request should have one extra Via (proxy's)"
                );
                prop_assert!(
                    forwarded_vias[0].contains("10.0.0.100:5060"),
                    "First Via should be proxy's address, got: {}",
                    forwarded_vias[0]
                );
                prop_assert!(
                    forwarded_vias[0].starts_with("SIP/2.0/UDP "),
                    "Proxy Via should start with SIP/2.0/UDP, got: {}",
                    forwarded_vias[0]
                );
                prop_assert!(
                    forwarded_vias[0].contains(";branch=z9hG4bK-proxy-"),
                    "Proxy Via should contain branch parameter, got: {}",
                    forwarded_vias[0]
                );

                // 2. Original Via headers are preserved in order
                for (i, original_via) in original_vias.iter().enumerate() {
                    prop_assert_eq!(
                        forwarded_vias[i + 1],
                        original_via.as_str(),
                        "Via at index {} should match original",
                        i + 1
                    );
                }

                // 3. Record-Route header is present with proxy's address
                let record_routes = forwarded_req.headers.get_all("Record-Route");
                prop_assert!(
                    !record_routes.is_empty(),
                    "Forwarded INVITE must contain a Record-Route header"
                );
                prop_assert!(
                    record_routes[0].contains("10.0.0.100:5060"),
                    "Record-Route must contain proxy address, got: {}",
                    record_routes[0]
                );
                prop_assert!(
                    record_routes[0].contains(";lr"),
                    "Record-Route must contain ;lr parameter, got: {}",
                    record_routes[0]
                );

                // 4. Original headers (Call-ID, From, To, CSeq) are preserved
                let fwd_call_id = forwarded_req.headers.get("Call-ID");
                let orig_call_id = request.headers.get("Call-ID");
                prop_assert_eq!(
                    fwd_call_id, orig_call_id,
                    "Call-ID should be preserved"
                );

                let fwd_from = forwarded_req.headers.get("From");
                let orig_from = request.headers.get("From");
                prop_assert_eq!(
                    fwd_from, orig_from,
                    "From header should be preserved"
                );

                let fwd_cseq = forwarded_req.headers.get("CSeq");
                let orig_cseq = request.headers.get("CSeq");
                prop_assert_eq!(
                    fwd_cseq, orig_cseq,
                    "CSeq header should be preserved"
                );

                Ok(())
            })?;
        }
    }

    // ===== Property 15: Viaヘッダのラウンドトリップ =====
    // Feature: sip-load-tester, Property 15: Viaヘッダのラウンドトリップ
    // **Validates: Requirements 19.2, 19.3**

    /// Strategy for generating a valid Via header value with random host/port/branch.
    fn arb_via_value() -> impl Strategy<Value = String> {
        (
            1u8..=254,  // host octets (avoid 0 and 255 for simplicity)
            1u8..=254,
            1u8..=254,
            1u8..=254,
            1024u16..60000,
            "[a-zA-Z0-9]{4,12}",  // branch suffix
        )
            .prop_map(|(a, b, c, d, port, branch)| {
                format!(
                    "SIP/2.0/UDP {}.{}.{}.{}:{};branch=z9hG4bK{}",
                    a, b, c, d, port, branch
                )
            })
    }

    proptest! {
        #[test]
        fn prop_via_header_roundtrip(
            original_vias in proptest::collection::vec(arb_via_value(), 1..5),
            call_id in "[a-zA-Z0-9]{1,32}",
        ) {
            // For any SIP request with arbitrary Via headers, after the proxy adds
            // its Via header (request forwarding) and then removes it (response
            // forwarding), the original Via headers are preserved.
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                let proxy = make_proxy(transport.clone());

                // Build a BYE request with Route header (so it bypasses LocationService
                // lookup and auth) carrying the generated Via headers.
                let mut headers = Headers::new();
                for via in &original_vias {
                    headers.add("Via", via.clone());
                }
                headers.set("Route", "<sip:10.0.0.2:5060;lr>".to_string());
                headers.set("From", "<sip:alice@example.com>;tag=t1".to_string());
                headers.set("To", "<sip:bob@example.com>;tag=t2".to_string());
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", "2 BYE".to_string());

                let request = SipMessage::Request(SipRequest {
                    method: Method::Bye,
                    request_uri: "sip:bob@example.com".to_string(),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                });

                // Step 1: Proxy forwards the request → adds its own Via at position 0
                proxy.handle_request(request, uac_addr()).await.unwrap();

                let sent_reqs = transport.sent_requests();
                prop_assert_eq!(
                    sent_reqs.len(), 1,
                    "Proxy should forward exactly one request"
                );
                let (forwarded_req, _dest) = &sent_reqs[0];

                // Verify proxy's Via was added at position 0
                let forwarded_vias = forwarded_req.headers.get_all("Via");
                prop_assert_eq!(
                    forwarded_vias.len(),
                    original_vias.len() + 1,
                    "Forwarded request should have one extra Via header (proxy's)"
                );
                // The first Via should be the proxy's
                prop_assert!(
                    forwarded_vias[0].contains("10.0.0.100:5060"),
                    "First Via should be proxy's address, got: {}",
                    forwarded_vias[0]
                );
                // The remaining Vias should match the originals
                for (i, original_via) in original_vias.iter().enumerate() {
                    prop_assert_eq!(
                        forwarded_vias[i + 1],
                        original_via.as_str(),
                        "Via at index {} should match original",
                        i + 1
                    );
                }

                // Step 2: Build a response with the same Via stack as the forwarded
                // request (proxy's Via on top, then original Vias).
                let mut resp_headers = Headers::new();
                for via in &forwarded_vias {
                    resp_headers.add("Via", via.to_string());
                }
                resp_headers.set("From", "<sip:alice@example.com>;tag=t1".to_string());
                resp_headers.set("To", "<sip:bob@example.com>;tag=t2".to_string());
                resp_headers.set("Call-ID", call_id.clone());
                resp_headers.set("CSeq", "2 BYE".to_string());

                let response = SipMessage::Response(SipResponse {
                    version: "SIP/2.0".to_string(),
                    status_code: 200,
                    reason_phrase: "OK".to_string(),
                    headers: resp_headers,
                    body: None,
                });

                // Step 3: Proxy forwards the response → removes its own Via (top)
                proxy.handle_response(response, uas_addr()).await.unwrap();

                let sent_resps = transport.sent_responses();
                // The first sent message was the request; the response is the second
                // sent message overall, but sent_responses filters only responses.
                prop_assert_eq!(
                    sent_resps.len(), 1,
                    "Proxy should forward exactly one response"
                );
                let (forwarded_resp, _dest) = &sent_resps[0];

                // Verify the response's Via headers match the original request's Vias
                let result_vias = forwarded_resp.headers.get_all("Via");
                prop_assert_eq!(
                    result_vias.len(),
                    original_vias.len(),
                    "After round-trip, Via count should match original ({} vs {})",
                    result_vias.len(),
                    original_vias.len()
                );
                for (i, original_via) in original_vias.iter().enumerate() {
                    prop_assert_eq!(
                        result_vias[i],
                        original_via.as_str(),
                        "Via at index {} should be preserved after round-trip",
                        i
                    );
                }

                Ok(())
            })?;
        }
    }

    // ===== Property 16: Record-Routeヘッダの挿入 =====
    // Feature: sip-load-tester, Property 16: Record-Routeヘッダの挿入
    // **Validates: Requirements 19.4**

    proptest! {
        #[test]
        fn prop_record_route_inserted_for_invite(
            call_id in "[a-zA-Z0-9]{1,32}",
        ) {
            // For any INVITE request forwarded through the proxy, the forwarded
            // request contains a Record-Route header with the proxy's address
            // and ;lr parameter.
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                let location_service = Arc::new(LocationService::new());

                // Pre-register the target URI at local domain so the proxy uses LocationService
                location_service.register(
                    "sip:bob@10.0.0.100",
                    ContactInfo {
                        contact_uri: "sip:bob@10.0.0.2:5060".to_string(),
                        address: uas_addr(),
                        expires: Instant::now() + std::time::Duration::from_secs(3600),
                    },
                );

                let proxy = make_proxy_with_location_service(
                    transport.clone(),
                    location_service,
                );

                // Build an INVITE request targeting local domain
                let mut headers = Headers::new();
                headers.add(
                    "Via",
                    "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string(),
                );
                headers.set("From", "<sip:alice@example.com>;tag=t1".to_string());
                headers.set("To", "<sip:bob@10.0.0.100>".to_string());
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", "1 INVITE".to_string());

                let request = SipMessage::Request(SipRequest {
                    method: Method::Invite,
                    request_uri: "sip:bob@10.0.0.100".to_string(),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                });

                proxy.handle_request(request, uac_addr()).await.unwrap();

                let sent_reqs = transport.sent_requests();
                prop_assert_eq!(
                    sent_reqs.len(), 1,
                    "Proxy should forward exactly one request"
                );
                let (forwarded_req, _dest) = &sent_reqs[0];

                // Verify Record-Route header is present
                let record_routes = forwarded_req.headers.get_all("Record-Route");
                prop_assert!(
                    !record_routes.is_empty(),
                    "Forwarded INVITE must contain a Record-Route header"
                );

                // Verify the Record-Route contains the proxy's address and ;lr
                let rr = record_routes[0];
                prop_assert!(
                    rr.contains("10.0.0.100:5060"),
                    "Record-Route must contain proxy address (10.0.0.100:5060), got: {}",
                    rr
                );
                prop_assert!(
                    rr.contains(";lr"),
                    "Record-Route must contain ;lr parameter, got: {}",
                    rr
                );

                Ok(())
            })?;
        }
    }

    // ===== Property 17: Location Serviceのラウンドトリップ =====
    // Feature: sip-load-tester, Property 17: Location Serviceのラウンドトリップ
    // **Validates: Requirements 19.6, 19.7**

    /// Strategy for generating a valid SIP AOR (Address of Record).
    fn arb_aor() -> impl Strategy<Value = String> {
        (
            "[a-z][a-z0-9]{0,15}",   // user part
            "[a-z][a-z0-9]{0,10}\\.[a-z]{2,4}", // domain part
        )
            .prop_map(|(user, domain)| format!("sip:{}@{}", user, domain))
    }

    /// Strategy for generating a valid SIP Contact URI.
    fn arb_contact_uri() -> impl Strategy<Value = String> {
        (
            "[a-z][a-z0-9]{0,15}",   // user part
            1u8..=254,
            1u8..=254,
            1u8..=254,
            1u8..=254,
            1024u16..60000,
        )
            .prop_map(|(user, a, b, c, d, port)| {
                format!("sip:{}@{}.{}.{}.{}:{}", user, a, b, c, d, port)
            })
    }

    /// Strategy for generating a valid SocketAddr.
    fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
        (1u8..=254, 1u8..=254, 1u8..=254, 1u8..=254, 1024u16..60000)
            .prop_map(|(a, b, c, d, port)| {
                SocketAddr::new(
                    std::net::IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d)),
                    port,
                )
            })
    }

    proptest! {
        #[test]
        fn prop_location_service_roundtrip(
            aor in arb_aor(),
            contact_uri in arb_contact_uri(),
            addr in arb_socket_addr(),
        ) {
            // For any AOR and Contact URI combination, after registering with
            // `register`, looking up with `lookup` returns the same Contact URI.
            let ls = LocationService::new();

            let contact = ContactInfo {
                contact_uri: contact_uri.clone(),
                address: addr,
                expires: Instant::now() + std::time::Duration::from_secs(3600),
            };

            ls.register(&aor, contact);

            let result = ls.lookup(&aor);
            prop_assert!(
                result.is_some(),
                "lookup should return Some after register for AOR: {}",
                aor
            );

            let looked_up = result.unwrap();
            prop_assert_eq!(
                &looked_up.contact_uri,
                &contact_uri,
                "Looked-up contact_uri should match registered contact_uri"
            );
            prop_assert_eq!(
                looked_up.address,
                addr,
                "Looked-up address should match registered address"
            );
        }
    }

    // ===== Property 18: 未登録URIのデフォルト転送 =====
    // Feature: sip-load-tester, Property 18: 未登録URIのデフォルト転送
    // **Validates: Requirements 19.8**

    /// Strategy for generating a random SIP Request-URI that will NOT be
    /// registered in an empty LocationService.
    fn arb_unregistered_request_uri() -> impl Strategy<Value = String> {
        (
            "[a-z][a-z0-9]{0,15}",   // user part
            "[a-z][a-z0-9]{0,10}\\.[a-z]{2,4}", // domain part
        )
            .prop_map(|(user, domain)| format!("sip:{}@{}", user, domain))
    }

    proptest! {
        #[test]
        fn prop_unregistered_uri_forwards_to_default(
            request_uri in arb_unregistered_request_uri(),
            call_id in "[a-zA-Z0-9]{1,32}",
            from_tag in "[a-zA-Z0-9]{1,16}",
        ) {
            // For any INVITE request whose Request-URI is not registered in the
            // LocationService, the proxy forwards to the default forward_addr
            // (not 404).
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                // Empty LocationService — no URIs registered
                let location_service = Arc::new(LocationService::new());
                let proxy = make_proxy_with_location_service(
                    transport.clone(),
                    location_service,
                );

                // Build an INVITE request with the random unregistered URI
                let mut headers = Headers::new();
                headers.add(
                    "Via",
                    "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string(),
                );
                headers.set(
                    "From",
                    format!("<sip:caller@example.com>;tag={}", from_tag),
                );
                headers.set("To", format!("<{}>", request_uri));
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", "1 INVITE".to_string());

                let request = SipMessage::Request(SipRequest {
                    method: Method::Invite,
                    request_uri: request_uri.clone(),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                });

                proxy.handle_request(request, uac_addr()).await.unwrap();

                // Verify: proxy should forward the request (not return 404)
                let sent_resps = transport.sent_responses();
                prop_assert_eq!(
                    sent_resps.len(), 0,
                    "Proxy should NOT send a 404 response for unregistered URI"
                );

                let sent_reqs = transport.sent_requests();
                prop_assert_eq!(
                    sent_reqs.len(), 1,
                    "Proxy should forward exactly one request"
                );

                let (forwarded_req, dest) = &sent_reqs[0];

                // Verify forwarded to default forward_addr
                prop_assert_eq!(
                    *dest,
                    uas_addr(),
                    "Request should be forwarded to default forward_addr"
                );

                // Verify Via header was added
                let vias = forwarded_req.headers.get_all("Via");
                prop_assert!(
                    vias.len() >= 2,
                    "Forwarded request should have proxy Via + original Via, got: {}",
                    vias.len()
                );
                prop_assert!(
                    vias[0].contains("10.0.0.100:5060"),
                    "First Via should be proxy's address, got: {}",
                    vias[0]
                );

                // Verify Record-Route was added for INVITE
                let record_routes = forwarded_req.headers.get_all("Record-Route");
                prop_assert!(
                    !record_routes.is_empty(),
                    "Forwarded INVITE must contain a Record-Route header"
                );
                prop_assert!(
                    record_routes[0].contains("10.0.0.100:5060"),
                    "Record-Route must contain proxy address"
                );

                // Verify Call-ID is preserved
                let fwd_call_id = forwarded_req.headers.get("Call-ID");
                prop_assert_eq!(
                    fwd_call_id,
                    Some(call_id.as_str()),
                    "Forwarded request Call-ID should match original"
                );

                Ok(())
            })?;
        }
    }

    // ===== Property 1: 受信リクエストログに必要情報がすべて含まれる =====
    // Feature: proxy-debug-logging, Property 1: 受信リクエストログに必要情報がすべて含まれる
    // **Validates: Requirements 2.1, 2.2, 7.1, 7.2**

    proptest! {
        #[test]
        fn prop_recv_req_log_contains_all_info(
            method in "[a-zA-Z]{1,16}",
            request_uri in "[a-zA-Z0-9:@\\./_\\-]{1,64}",
            addr in arb_socket_addr(),
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
        ) {
            let result = format_recv_req_log(&method, &request_uri, addr, &call_id);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "RECV_REQ"
            prop_assert!(
                result.contains("RECV_REQ"),
                "Log should contain RECV_REQ, got: {}",
                result
            );

            // Assert the output contains the method name
            prop_assert!(
                result.contains(&method),
                "Log should contain method '{}', got: {}",
                method, result
            );

            // Assert the output contains the request URI
            prop_assert!(
                result.contains(&request_uri),
                "Log should contain request_uri '{}', got: {}",
                request_uri, result
            );

            // Assert the output contains the address (as string)
            let addr_str = addr.to_string();
            prop_assert!(
                result.contains(&addr_str),
                "Log should contain address '{}', got: {}",
                addr_str, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );
        }
    }

    // ===== Property 2: 転送リクエストログに必要情報がすべて含まれる =====
    // Feature: proxy-debug-logging, Property 2: 転送リクエストログに必要情報がすべて含まれる
    // **Validates: Requirements 3.1, 3.2, 7.1, 7.2**

    proptest! {
        #[test]
        fn prop_fwd_req_log_contains_all_info(
            method in "[a-zA-Z]{1,16}",
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
            dest in arb_socket_addr(),
        ) {
            let result = format_fwd_req_log(&method, &call_id, dest);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "FWD_REQ"
            prop_assert!(
                result.contains("FWD_REQ"),
                "Log should contain FWD_REQ, got: {}",
                result
            );

            // Assert the output contains the method name
            prop_assert!(
                result.contains(&method),
                "Log should contain method '{}', got: {}",
                method, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );

            // Assert the output contains the destination address
            let dest_str = dest.to_string();
            prop_assert!(
                result.contains(&dest_str),
                "Log should contain dest '{}', got: {}",
                dest_str, result
            );
        }
    }

    // ===== Property 3: 受信レスポンスログに必要情報がすべて含まれる =====
    // Feature: proxy-debug-logging, Property 3: 受信レスポンスログに必要情報がすべて含まれる
    // **Validates: Requirements 4.1, 4.2, 7.1, 7.2**

    proptest! {
        #[test]
        fn prop_recv_res_log_contains_all_info(
            status_code in 100u16..=699u16,
            reason in "[a-zA-Z ]{1,32}",
            addr in arb_socket_addr(),
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
        ) {
            let result = format_recv_res_log(status_code, &reason, addr, &call_id);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "RECV_RES"
            prop_assert!(
                result.contains("RECV_RES"),
                "Log should contain RECV_RES, got: {}",
                result
            );

            // Assert the output contains the status code (as string)
            let status_str = status_code.to_string();
            prop_assert!(
                result.contains(&status_str),
                "Log should contain status_code '{}', got: {}",
                status_str, result
            );

            // Assert the output contains the reason phrase
            prop_assert!(
                result.contains(&reason),
                "Log should contain reason '{}', got: {}",
                reason, result
            );

            // Assert the output contains the address (as string)
            let addr_str = addr.to_string();
            prop_assert!(
                result.contains(&addr_str),
                "Log should contain address '{}', got: {}",
                addr_str, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );
        }
    }

    // ===== Property 4: 転送レスポンスログに必要情報がすべて含まれる =====
    // Feature: proxy-debug-logging, Property 4: 転送レスポンスログに必要情報がすべて含まれる
    // **Validates: Requirements 5.1, 5.2, 7.1, 7.2**

    proptest! {
        #[test]
        fn prop_fwd_res_log_contains_all_info(
            status_code in 100u16..=699u16,
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
            dest in arb_socket_addr(),
        ) {
            let result = format_fwd_res_log(status_code, &call_id, dest);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "FWD_RES"
            prop_assert!(
                result.contains("FWD_RES"),
                "Log should contain FWD_RES, got: {}",
                result
            );

            // Assert the output contains the status code (as string)
            let status_str = status_code.to_string();
            prop_assert!(
                result.contains(&status_str),
                "Log should contain status_code '{}', got: {}",
                status_str, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );

            // Assert the output contains the destination address
            let dest_str = dest.to_string();
            prop_assert!(
                result.contains(&dest_str),
                "Log should contain dest '{}', got: {}",
                dest_str, result
            );
        }
    }

    // ===== Property 5: エラーログに必要情報がすべて含まれる =====
    // Feature: proxy-debug-logging, Property 5: エラーログに必要情報がすべて含まれる
    // **Validates: Requirements 6.1, 6.2, 7.1, 7.2**

    proptest! {
        #[test]
        fn prop_req_error_log_contains_all_info(
            error in "[a-zA-Z0-9 _\\-\\.]{1,64}",
            method in "[a-zA-Z]{1,16}",
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
            dest in arb_socket_addr(),
        ) {
            let result = format_req_error_log(&error, &method, &call_id, dest);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "ERROR"
            prop_assert!(
                result.contains("ERROR"),
                "Log should contain ERROR, got: {}",
                result
            );

            // Assert the output contains the error message
            prop_assert!(
                result.contains(&error),
                "Log should contain error '{}', got: {}",
                error, result
            );

            // Assert the output contains the method name
            prop_assert!(
                result.contains(&method),
                "Log should contain method '{}', got: {}",
                method, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );

            // Assert the output contains the destination address
            let dest_str = dest.to_string();
            prop_assert!(
                result.contains(&dest_str),
                "Log should contain dest '{}', got: {}",
                dest_str, result
            );
        }
    }

    proptest! {
        #[test]
        fn prop_res_error_log_contains_all_info(
            error in "[a-zA-Z0-9 _\\-\\.]{1,64}",
            status_code in 100u16..=699u16,
            call_id in "[a-zA-Z0-9@\\.\\-_]{1,64}",
            dest in arb_socket_addr(),
        ) {
            let result = format_res_error_log(&error, status_code, &call_id, dest);

            // Assert the output starts with "[DEBUG]"
            prop_assert!(
                result.starts_with("[DEBUG]"),
                "Log should start with [DEBUG], got: {}",
                result
            );

            // Assert the output contains "ERROR"
            prop_assert!(
                result.contains("ERROR"),
                "Log should contain ERROR, got: {}",
                result
            );

            // Assert the output contains the error message
            prop_assert!(
                result.contains(&error),
                "Log should contain error '{}', got: {}",
                error, result
            );

            // Assert the output contains the status code (as string)
            let status_str = status_code.to_string();
            prop_assert!(
                result.contains(&status_str),
                "Log should contain status_code '{}', got: {}",
                status_str, result
            );

            // Assert the output contains the call-id
            prop_assert!(
                result.contains(&call_id),
                "Log should contain call_id '{}', got: {}",
                call_id, result
            );

            // Assert the output contains the destination address
            let dest_str = dest.to_string();
            prop_assert!(
                result.contains(&dest_str),
                "Log should contain dest '{}', got: {}",
                dest_str, result
            );
        }
    }

    // ===== Property 6: Branch値の一意性テスト =====
    // Feature: performance-bottleneck-optimization, Property 6
    // **Validates: Requirements 5.2**

    proptest! {
        /// Feature: performance-bottleneck-optimization, Property 6
        /// N個（N>=2）のbranch値生成で全値が一意
        /// **Validates: Requirements 5.2**
        #[test]
        fn prop_branch_uniqueness(n in 2u32..=100) {
            let mut branches = std::collections::HashSet::new();
            for _ in 0..n {
                let branch = rand_branch();
                branches.insert(branch);
            }
            prop_assert_eq!(
                branches.len(),
                n as usize,
                "All {} generated branch values must be unique, but got {} unique values",
                n,
                branches.len()
            );
        }
    }

    // ===== Property 7: プロキシレスポンス転送のVia除去正確性テスト =====
    // Feature: performance-bottleneck-optimization, Property 7
    // **Validates: Requirements 5.3**

    proptest! {
        /// Feature: performance-bottleneck-optimization, Property 7
        /// 2つ以上のViaヘッダを持つレスポンスに対して、転送後にプロキシのViaが除去される
        /// **Validates: Requirements 5.3**
        #[test]
        fn prop_proxy_response_via_removal(
            remaining_vias in proptest::collection::vec(arb_via_value(), 1..5),
            status_code in 100u16..=699u16,
            reason in "[A-Za-z ]{1,20}",
            call_id in "[a-zA-Z0-9]{1,32}",
            from_tag in "[a-zA-Z0-9]{1,16}",
            proxy_branch_suffix in "[a-f0-9]{8,16}",
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                let proxy = make_proxy(transport.clone());

                // Build a response with proxy's Via on top, followed by remaining Vias
                let proxy_via = format!(
                    "SIP/2.0/UDP 10.0.0.100:5060;branch=z9hG4bK-proxy-{}",
                    proxy_branch_suffix
                );

                let mut headers = Headers::new();
                // Proxy's Via is the topmost
                headers.add("Via", proxy_via.clone());
                // Then the remaining Via headers
                for via in &remaining_vias {
                    headers.add("Via", via.clone());
                }
                headers.set("From", format!("<sip:alice@example.com>;tag={}", from_tag));
                headers.set("To", "<sip:bob@example.com>;tag=resp1".to_string());
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", "1 INVITE".to_string());

                let response = SipMessage::Response(SipResponse {
                    version: "SIP/2.0".to_string(),
                    status_code,
                    reason_phrase: reason.clone(),
                    headers,
                    body: None,
                });

                proxy.handle_response(response, uas_addr()).await.unwrap();

                let sent_resps = transport.sent_responses();
                prop_assert_eq!(
                    sent_resps.len(), 1,
                    "Proxy should forward exactly one response"
                );
                let (forwarded_resp, dest) = &sent_resps[0];

                // 1. Proxy's Via is removed
                let forwarded_vias = forwarded_resp.headers.get_all("Via");
                prop_assert_eq!(
                    forwarded_vias.len(),
                    remaining_vias.len(),
                    "Forwarded response should have proxy's Via removed ({} vs expected {})",
                    forwarded_vias.len(),
                    remaining_vias.len()
                );

                // 2. Remaining Via headers are preserved in order
                for (i, original_via) in remaining_vias.iter().enumerate() {
                    prop_assert_eq!(
                        forwarded_vias[i],
                        original_via.as_str(),
                        "Via at index {} should match original after proxy Via removal",
                        i
                    );
                }

                // 3. Proxy's Via is not present in forwarded response
                for via in &forwarded_vias {
                    prop_assert!(
                        !via.contains("10.0.0.100:5060"),
                        "Proxy's Via should not be present in forwarded response, got: {}",
                        via
                    );
                }

                // 4. Response is forwarded to the address from the now-top Via header
                let expected_dest = parse_via_addr(&remaining_vias[0]).unwrap();
                prop_assert_eq!(
                    *dest,
                    expected_dest,
                    "Response should be forwarded to address from the next Via header"
                );

                // 5. Other headers are preserved
                let fwd_call_id = forwarded_resp.headers.get("Call-ID");
                prop_assert_eq!(
                    fwd_call_id,
                    Some(call_id.as_str()),
                    "Call-ID should be preserved"
                );

                Ok(())
            })?;
        }
    }
}
