// UAC (User Agent Client) module
//
// Handles outgoing SIP requests (REGISTER, INVITE-BYE scenarios).
// Uses SipTransport trait for testability, UserPool for user selection,
// DialogManager for dialog tracking, and StatsCollector for statistics.

pub mod load_pattern;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::DigestAuth;
use crate::dialog::{Dialog, DialogManager, DialogState};
use crate::error::SipLoadTestError;
use crate::sip::formatter::format_sip_message;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
use crate::stats::StatsCollector;
use crate::uas::SipTransport;
use crate::user_pool::{UserEntry, UserPool};

/// UAC configuration
#[derive(Debug, Clone)]
pub struct UacConfig {
    pub proxy_addr: SocketAddr,
    pub local_addr: SocketAddr,
    pub call_duration: Duration,
    pub dialog_timeout: Duration,
}

impl Default for UacConfig {
    fn default() -> Self {
        Self {
            proxy_addr: "127.0.0.1:5060".parse().unwrap(),
            local_addr: "127.0.0.1:5060".parse().unwrap(),
            call_duration: Duration::from_secs(3),
            dialog_timeout: Duration::from_secs(32),
        }
    }
}

/// Result of background REGISTER operation.
#[derive(Debug, Clone, PartialEq)]
pub struct BgRegisterResult {
    pub success: u32,
    pub failed: u32,
}

/// UAC (User Agent Client) - generates and sends SIP requests.
pub struct Uac {
    transport: Arc<dyn SipTransport>,
    dialog_manager: Arc<DialogManager>,
    stats: Arc<StatsCollector>,
    pub auth: Option<DigestAuth>,
    user_pool: Arc<UserPool>,
    config: UacConfig,
}

impl Uac {
    /// Create a new UAC instance.
    pub fn new(
        transport: Arc<dyn SipTransport>,
        dialog_manager: Arc<DialogManager>,
        stats: Arc<StatsCollector>,
        user_pool: Arc<UserPool>,
        config: UacConfig,
    ) -> Self {
        Self {
            transport,
            dialog_manager,
            stats,
            auth: None,
            user_pool,
            config,
        }
    }

    /// Send a REGISTER request. Selects the next user from UserPool via round-robin.
    pub async fn send_register(&self) -> Result<(), SipLoadTestError> {
        let user = self.user_pool.next_user();
        let dialog = self.dialog_manager.create_dialog()?;

        let request = self.build_register_request(&dialog, user);
        let bytes = format_sip_message(&SipMessage::Request(request.clone()));

        // Store original request in dialog for potential re-send (auth)
        if let Some(mut d) = self.dialog_manager.get_dialog_mut(&dialog.call_id) {
            d.state = DialogState::Trying;
            d.original_request = Some(SipMessage::Request(request));
        }

        self.stats.increment_active_dialogs();
        self.transport.send_to(&bytes, self.config.proxy_addr).await?;

        Ok(())
    }

    /// Send an INVITE request. Selects the next user from UserPool via round-robin.
    pub async fn send_invite(&self) -> Result<(), SipLoadTestError> {
        let user = self.user_pool.next_user();
        let dialog = self.dialog_manager.create_dialog()?;

        let request = self.build_invite_request(&dialog, user);
        let bytes = format_sip_message(&SipMessage::Request(request.clone()));

        if let Some(mut d) = self.dialog_manager.get_dialog_mut(&dialog.call_id) {
            d.state = DialogState::Trying;
            d.original_request = Some(SipMessage::Request(request));
        }

        self.stats.increment_active_dialogs();
        self.transport.send_to(&bytes, self.config.proxy_addr).await?;

        Ok(())
    }

    /// Send background REGISTER requests before load test.
    /// Sends `count` REGISTER requests and waits for responses or timeout.
    /// Returns the number of successful and failed registrations.
    pub async fn bg_register(&self, count: u32, timeout: Duration) -> BgRegisterResult {
        if count == 0 {
            return BgRegisterResult { success: 0, failed: 0 };
        }

        // Record initial stats baseline
        let snapshot_before = self.stats.snapshot();
        let success_before = snapshot_before.successful_calls;
        let failed_before = snapshot_before.failed_calls;

        // Send all REGISTER requests
        let mut _sent = 0u32;
        for _ in 0..count {
            if self.send_register().await.is_ok() {
                _sent += 1;
            }
        }

        // Wait for all dialogs to complete or timeout
        let deadline = Instant::now() + timeout;
        loop {
            if self.dialog_manager.active_count() == 0 {
                break;
            }
            if Instant::now() >= deadline {
                // Timeout: clean up remaining dialogs
                let _timed_out = self.check_timeouts_force();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Calculate results from stats delta
        let snapshot_after = self.stats.snapshot();
        let success = (snapshot_after.successful_calls - success_before) as u32;
        let failed = (snapshot_after.failed_calls - failed_before) as u32;

        BgRegisterResult { success, failed }
    }

    /// Force-timeout all remaining active dialogs (used by bg_register on timeout).
    fn check_timeouts_force(&self) -> usize {
        let call_ids = self.dialog_manager.all_call_ids();
        let mut timed_out = 0;

        for call_id in call_ids {
            let should_remove = {
                if let Some(dialog) = self.dialog_manager.get_dialog(&call_id) {
                    dialog.state != DialogState::Terminated
                        && dialog.state != DialogState::Error
                } else {
                    false
                }
            };

            if should_remove {
                self.dialog_manager.remove_dialog(&call_id);
                self.stats.record_failure();
                self.stats.decrement_active_dialogs();
                timed_out += 1;
            }
        }

        timed_out
    }

    /// Handle a received SIP response. Dispatches to the correct dialog by Call-ID.
    pub async fn handle_response(
        &self,
        response: SipMessage,
        _from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        let resp = match &response {
            SipMessage::Response(r) => r,
            SipMessage::Request(_) => return Ok(()), // Ignore requests
        };

        let call_id = resp.headers.get("Call-ID").unwrap_or("").to_string();
        if call_id.is_empty() {
            return Ok(());
        }

        // Check if dialog exists
        let dialog_exists = self.dialog_manager.get_dialog(&call_id).is_some();
        if !dialog_exists {
            return Ok(()); // Unknown Call-ID, ignore
        }

        // Determine the method from CSeq header
        let method = self.extract_method_from_cseq(resp);

        match (method.as_deref(), resp.status_code) {
            // REGISTER 200 OK → mark complete, record success
            (Some("REGISTER"), 200) => {
                self.handle_register_success(&call_id, resp).await?;
            }
            // INVITE 200 OK → send ACK, schedule BYE after call_duration
            (Some("INVITE"), 200) => {
                self.handle_invite_success(&call_id, resp).await?;
            }
            // BYE 200 OK → mark terminated, record success
            (Some("BYE"), 200) => {
                self.handle_bye_success(&call_id, resp).await?;
            }
            // 401 Unauthorized → re-send with Authorization header
            (_, 401) | (_, 407) => {
                self.handle_auth_challenge(&call_id, resp, &response).await?;
            }
            _ => {
                // 4xx/5xx/6xx エラーレスポンス → 失敗として記録
                if resp.status_code >= 400 {
                    self.stats.record_failure();
                    self.dialog_manager.remove_dialog(&call_id);
                    self.stats.decrement_active_dialogs();
                }
                // 1xx provisional responses are ignored (no action needed)
            }
        }

        Ok(())
    }

    /// Check for timed-out dialogs and record failures.
    /// Returns the number of timed-out dialogs.
    pub fn check_timeouts(&self) -> usize {
        let now = Instant::now();
        let timeout = self.config.dialog_timeout;
        let call_ids = self.dialog_manager.all_call_ids();
        let mut timed_out = 0;

        for call_id in call_ids {
            let should_timeout = {
                if let Some(dialog) = self.dialog_manager.get_dialog(&call_id) {
                    let elapsed = now.duration_since(dialog.created_at);
                    elapsed >= timeout
                        && dialog.state != DialogState::Terminated
                        && dialog.state != DialogState::Error
                } else {
                    false
                }
            };

            if should_timeout {
                self.dialog_manager.remove_dialog(&call_id);
                self.stats.record_failure();
                self.stats.decrement_active_dialogs();
                timed_out += 1;
            }
        }

        timed_out
    }

    /// Shutdown the UAC: send BYE for all active (Confirmed) dialogs.
    pub async fn shutdown(&self) -> Result<(), SipLoadTestError> {
        let call_ids = self.dialog_manager.all_call_ids();

        for call_id in call_ids {
            let dialog_info = {
                self.dialog_manager.get_dialog(&call_id).map(|d| {
                    (d.call_id.clone(), d.from_tag.clone(), d.to_tag.clone(), d.cseq, d.state.clone())
                })
            };

            if let Some((cid, from_tag, to_tag, cseq, state)) = dialog_info {
                if state == DialogState::Confirmed {
                    let bye = self.build_bye_request(&cid, &from_tag, to_tag.as_deref(), cseq + 1);
                    let bytes = format_sip_message(&SipMessage::Request(bye));
                    let _ = self.transport.send_to(&bytes, self.config.proxy_addr).await;
                }
                self.dialog_manager.remove_dialog(&cid);
                self.stats.decrement_active_dialogs();
            }
        }

        Ok(())
    }

    // --- Private helper methods ---

    /// Handle REGISTER 200 OK: mark dialog complete, record success.
    async fn handle_register_success(
        &self,
        call_id: &str,
        resp: &SipResponse,
    ) -> Result<(), SipLoadTestError> {
        let latency = {
            if let Some(dialog) = self.dialog_manager.get_dialog(call_id) {
                Instant::now().duration_since(dialog.created_at)
            } else {
                Duration::ZERO
            }
        };

        self.dialog_manager.remove_dialog(call_id);
        self.stats.record_call(resp.status_code, latency);
        self.stats.decrement_active_dialogs();

        Ok(())
    }

    /// Handle INVITE 200 OK: send ACK, update dialog to Confirmed, schedule BYE.
    /// If Session-Expires header is present, record it and schedule re-INVITE at half expiry.
    async fn handle_invite_success(
        &self,
        call_id: &str,
        resp: &SipResponse,
    ) -> Result<(), SipLoadTestError> {
        // Extract To tag from response
        let to_tag = resp.headers.get("To")
            .and_then(|to| {
                to.split(';')
                    .find(|p| p.trim().starts_with("tag="))
                    .map(|p| p.trim().trim_start_matches("tag=").to_string())
            });

        // Parse Session-Expires header if present
        let session_expires = resp
            .headers
            .get("Session-Expires")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);

        // Get dialog info for building ACK
        let (from_tag, cseq) = {
            if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
                dialog.state = DialogState::Confirmed;
                if let Some(ref tag) = to_tag {
                    dialog.to_tag = Some(tag.clone());
                }
                // Record session_expires in dialog
                if let Some(se) = session_expires {
                    dialog.session_expires = Some(se);
                }
                (dialog.from_tag.clone(), dialog.cseq)
            } else {
                return Ok(());
            }
        };

        // Send ACK
        let ack = self.build_ack_request(call_id, &from_tag, to_tag.as_deref(), cseq);
        let ack_bytes = format_sip_message(&SipMessage::Request(ack));
        self.transport.send_to(&ack_bytes, self.config.proxy_addr).await?;

        // Schedule re-INVITE at session_expires/2 if Session-Expires is present
        if let Some(se) = session_expires {
            let reinvite_delay = se / 2;
            let transport = self.transport.clone();
            let dialog_manager = self.dialog_manager.clone();
            let proxy_addr = self.config.proxy_addr;
            let local_addr = self.config.local_addr;
            let call_id_owned = call_id.to_string();

            tokio::spawn(async move {
                tokio::time::sleep(reinvite_delay).await;

                let dialog_info = {
                    dialog_manager.get_dialog(&call_id_owned).map(|d| {
                        (d.from_tag.clone(), d.to_tag.clone(), d.cseq, d.state.clone())
                    })
                };

                if let Some((from_tag, to_tag, cseq, state)) = dialog_info {
                    if state == DialogState::Confirmed {
                        let reinvite = build_reinvite_request_static(
                            &call_id_owned,
                            &from_tag,
                            to_tag.as_deref(),
                            cseq + 1,
                            local_addr,
                        );
                        let reinvite_bytes = format_sip_message(&SipMessage::Request(reinvite));
                        let _ = transport.send_to(&reinvite_bytes, proxy_addr).await;

                        // Update CSeq in dialog
                        if let Some(mut d) = dialog_manager.get_dialog_mut(&call_id_owned) {
                            d.cseq = cseq + 1;
                        }
                    }
                }
            });
        }

        // Schedule BYE after call_duration using tokio::spawn
        let transport = self.transport.clone();
        let dialog_manager = self.dialog_manager.clone();
        let _stats = self.stats.clone();
        let call_duration = self.config.call_duration;
        let proxy_addr = self.config.proxy_addr;
        let local_addr = self.config.local_addr;
        let call_id_owned = call_id.to_string();

        tokio::spawn(async move {
            tokio::time::sleep(call_duration).await;

            let dialog_info = {
                dialog_manager.get_dialog(&call_id_owned).map(|d| {
                    (d.from_tag.clone(), d.to_tag.clone(), d.cseq, d.state.clone(), d.created_at)
                })
            };

            if let Some((from_tag, to_tag, cseq, state, _created_at)) = dialog_info {
                if state == DialogState::Confirmed {
                    let bye = build_bye_request_static(
                        &call_id_owned,
                        &from_tag,
                        to_tag.as_deref(),
                        cseq + 1,
                        local_addr,
                    );
                    let bye_bytes = format_sip_message(&SipMessage::Request(bye));
                    let _ = transport.send_to(&bye_bytes, proxy_addr).await;

                    // Update dialog state to Trying (waiting for BYE response)
                    if let Some(mut d) = dialog_manager.get_dialog_mut(&call_id_owned) {
                        d.state = DialogState::Trying;
                        d.cseq = cseq + 1;
                    }
                }
            }
        });

        Ok(())
    }

    /// Handle BYE 200 OK: mark dialog terminated, record success.
    async fn handle_bye_success(
        &self,
        call_id: &str,
        resp: &SipResponse,
    ) -> Result<(), SipLoadTestError> {
        let latency = {
            if let Some(dialog) = self.dialog_manager.get_dialog(call_id) {
                Instant::now().duration_since(dialog.created_at)
            } else {
                Duration::ZERO
            }
        };

        self.dialog_manager.remove_dialog(call_id);
        self.stats.record_call(resp.status_code, latency);
        self.stats.decrement_active_dialogs();

        Ok(())
    }

    /// Handle 401/407 auth challenge: re-send original request with auth header.
    /// If auth_attempted is already true, record failure (infinite loop prevention).
    /// If no auth module is configured, record failure immediately.
    async fn handle_auth_challenge(
        &self,
        call_id: &str,
        resp: &SipResponse,
        response: &SipMessage,
    ) -> Result<(), SipLoadTestError> {
        // Check if auth was already attempted (infinite loop prevention)
        let auth_already_attempted = {
            if let Some(dialog) = self.dialog_manager.get_dialog(call_id) {
                dialog.auth_attempted
            } else {
                return Ok(());
            }
        };

        if auth_already_attempted {
            // Second auth challenge → failure, do not re-send
            self.stats.record_auth_failure();
            self.stats.record_failure();
            if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
                dialog.state = DialogState::Error;
            }
            return Ok(());
        }

        // Check if auth module is available
        let auth = match &self.auth {
            Some(a) => a,
            None => {
                // No auth module → record failure
                self.stats.record_auth_failure();
                self.stats.record_failure();
                if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
                    dialog.state = DialogState::Error;
                }
                return Ok(());
            }
        };

        // Parse the challenge from the response
        let challenge = match DigestAuth::parse_challenge(response) {
            Ok(c) => c,
            Err(_) => {
                self.stats.record_auth_failure();
                self.stats.record_failure();
                if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
                    dialog.state = DialogState::Error;
                }
                return Ok(());
            }
        };

        // Get original request and extract username from From header
        let (original_request, current_cseq) = {
            if let Some(dialog) = self.dialog_manager.get_dialog(call_id) {
                (dialog.original_request.clone(), dialog.cseq)
            } else {
                return Ok(());
            }
        };

        let original_request = match original_request {
            Some(req) => req,
            None => return Ok(()),
        };

        let orig_req = match &original_request {
            SipMessage::Request(r) => r,
            _ => return Ok(()),
        };

        // Extract username from From header: <sip:username@domain>;tag=xxx
        let from_header = orig_req.headers.get("From").unwrap_or("");
        let username = Self::extract_username_from_uri(from_header);

        // Look up user in UserPool
        let user = match username.and_then(|u| auth.user_pool.find_by_username(u)) {
            Some(u) => u.clone(),
            None => {
                self.stats.record_auth_failure();
                self.stats.record_failure();
                if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
                    dialog.state = DialogState::Error;
                }
                return Ok(());
            }
        };

        // Determine header name based on status code
        let header_name = if resp.status_code == 401 {
            "Authorization"
        } else {
            "Proxy-Authorization"
        };

        // Extract method string from original request
        let method_str = match &orig_req.method {
            Method::Register => "REGISTER",
            Method::Invite => "INVITE",
            Method::Ack => "ACK",
            Method::Bye => "BYE",
            Method::Options => "OPTIONS",
            Method::Update => "UPDATE",
            Method::Other(s) => s.as_str(),
        };

        // Build authorization header value
        let auth_value = auth.create_authorization_header(
            &user,
            &challenge,
            method_str,
            &orig_req.request_uri,
        );

        // Build new request with incremented CSeq and auth header
        let new_cseq = current_cseq + 1;
        let mut new_req = orig_req.clone();
        new_req.headers.set("CSeq", format!("{} {}", new_cseq, method_str));
        new_req.headers.set(header_name, auth_value);

        let bytes = format_sip_message(&SipMessage::Request(new_req.clone()));

        // Update dialog: set auth_attempted, increment CSeq, store new original_request
        if let Some(mut dialog) = self.dialog_manager.get_dialog_mut(call_id) {
            dialog.auth_attempted = true;
            dialog.cseq = new_cseq;
            dialog.original_request = Some(SipMessage::Request(new_req));
        }

        self.transport.send_to(&bytes, self.config.proxy_addr).await?;

        Ok(())
    }

    /// Extract username from a SIP URI in a From/To header.
    /// E.g., "<sip:alice@example.com>;tag=abc" → Some("alice")
    fn extract_username_from_uri(header_value: &str) -> Option<&str> {
        // Find "sip:" prefix, then extract until '@'
        let sip_start = header_value.find("sip:")?;
        let after_sip = &header_value[sip_start + 4..];
        let at_pos = after_sip.find('@')?;
        Some(&after_sip[..at_pos])
    }

    /// Extract method name from CSeq header (e.g., "1 INVITE" → "INVITE")
    fn extract_method_from_cseq(&self, resp: &SipResponse) -> Option<String> {
        resp.headers.get("CSeq").and_then(|cseq| {
            cseq.split_whitespace().nth(1).map(|m| m.to_string())
        })
    }

    /// Build a REGISTER request
    fn build_register_request(&self, dialog: &Dialog, user: &UserEntry) -> SipRequest {
        let mut headers = Headers::new();
        headers.add(
            "Via",
            format!("SIP/2.0/UDP {};branch={}", self.config.local_addr, crate::sip::generate_branch(&dialog.call_id, dialog.cseq, "REGISTER")),
        );
        headers.set(
            "From",
            format!("<sip:{}@{}>;tag={}", user.username, user.domain, dialog.from_tag),
        );
        headers.set(
            "To",
            format!("<sip:{}@{}>", user.username, user.domain),
        );
        headers.set("Call-ID", dialog.call_id.clone());
        headers.set("CSeq", format!("{} REGISTER", dialog.cseq));
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Content-Length", "0".to_string());

        SipRequest {
            method: Method::Register,
            request_uri: format!("sip:{}", user.domain),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        }
    }

    /// Build an INVITE request
    fn build_invite_request(&self, dialog: &Dialog, user: &UserEntry) -> SipRequest {
        let mut headers = Headers::new();
        headers.add(
            "Via",
            format!("SIP/2.0/UDP {};branch={}", self.config.local_addr, crate::sip::generate_branch(&dialog.call_id, dialog.cseq, "INVITE")),
        );
        headers.set(
            "From",
            format!("<sip:{}@{}>;tag={}", user.username, user.domain, dialog.from_tag),
        );
        headers.set(
            "To",
            format!("<sip:{}@{}>", user.username, user.domain),
        );
        headers.set("Call-ID", dialog.call_id.clone());
        headers.set("CSeq", format!("{} INVITE", dialog.cseq));
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Content-Length", "0".to_string());

        SipRequest {
            method: Method::Invite,
            request_uri: format!("sip:{}@{}", user.username, user.domain),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        }
    }

    /// Build an ACK request
    fn build_ack_request(
        &self,
        call_id: &str,
        from_tag: &str,
        to_tag: Option<&str>,
        cseq: u32,
    ) -> SipRequest {
        let mut headers = Headers::new();
        headers.add(
            "Via",
            format!("SIP/2.0/UDP {};branch={}", self.config.local_addr, crate::sip::generate_branch(call_id, cseq, "ACK")),
        );
        headers.set("Call-ID", call_id.to_string());
        headers.set("From", format!("<sip:uac@local>;tag={}", from_tag));
        let to_val = if let Some(tag) = to_tag {
            format!("<sip:uas@remote>;tag={}", tag)
        } else {
            "<sip:uas@remote>".to_string()
        };
        headers.set("To", to_val);
        headers.set("CSeq", format!("{} ACK", cseq));
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Content-Length", "0".to_string());

        SipRequest {
            method: Method::Ack,
            request_uri: "sip:uas@remote".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        }
    }

    /// Build a BYE request
    fn build_bye_request(
        &self,
        call_id: &str,
        from_tag: &str,
        to_tag: Option<&str>,
        cseq: u32,
    ) -> SipRequest {
        build_bye_request_static(call_id, from_tag, to_tag, cseq, self.config.local_addr)
    }
}

/// Static re-INVITE builder for use in spawned tasks (no &self reference needed)
fn build_reinvite_request_static(
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    cseq: u32,
    local_addr: SocketAddr,
) -> SipRequest {
    let mut headers = Headers::new();
    headers.add(
        "Via",
        format!("SIP/2.0/UDP {};branch={}", local_addr, crate::sip::generate_branch(call_id, cseq, "INVITE")),
    );
    headers.set("Call-ID", call_id.to_string());
    headers.set("From", format!("<sip:uac@local>;tag={}", from_tag));
    let to_val = if let Some(tag) = to_tag {
        format!("<sip:uas@remote>;tag={}", tag)
    } else {
        "<sip:uas@remote>".to_string()
    };
    headers.set("To", to_val);
    headers.set("CSeq", format!("{} INVITE", cseq));
    headers.set("Max-Forwards", "70".to_string());
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
fn build_bye_request_static(
    call_id: &str,
    from_tag: &str,
    to_tag: Option<&str>,
    cseq: u32,
    local_addr: SocketAddr,
) -> SipRequest {
    let mut headers = Headers::new();
    headers.add(
        "Via",
        format!("SIP/2.0/UDP {};branch={}", local_addr, crate::sip::generate_branch(call_id, cseq, "BYE")),
    );
    headers.set("Call-ID", call_id.to_string());
    headers.set("From", format!("<sip:uac@local>;tag={}", from_tag));
    let to_val = if let Some(tag) = to_tag {
        format!("<sip:uas@remote>;tag={}", tag)
    } else {
        "<sip:uas@remote>".to_string()
    };
    headers.set("To", to_val);
    headers.set("CSeq", format!("{} BYE", cseq));
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
    use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
    use crate::sip::parser::parse_sip_message;
    use crate::stats::StatsCollector;
    use crate::user_pool::{UserPool, UsersFile, UserEntry};
    use crate::dialog::{Dialog, DialogManager, DialogState};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use proptest::prelude::*;

    // ===== proptest helpers =====

    /// Generate an arbitrary SocketAddr (IPv4) for proptest
    fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
        (any::<[u8; 4]>(), 1024u16..65535)
            .prop_map(|(ip, port)| {
                let addr = std::net::Ipv4Addr::new(ip[0], ip[1], ip[2], ip[3]);
                SocketAddr::new(std::net::IpAddr::V4(addr), port)
            })
    }

    /// Generate a pair of distinct SocketAddrs (proxy_addr != local_addr)
    fn arb_distinct_addr_pair() -> impl Strategy<Value = (SocketAddr, SocketAddr)> {
        (arb_socket_addr(), arb_socket_addr())
            .prop_filter("proxy_addr must differ from local_addr", |(a, b)| a != b)
    }

    /// Generate a valid call_id (at least 16 hex chars for branch extraction)
    fn arb_call_id() -> impl Strategy<Value = String> {
        "[0-9a-f]{16,32}".prop_map(|s| s)
    }

    /// Generate a valid from_tag
    fn arb_from_tag() -> impl Strategy<Value = String> {
        "[a-z0-9]{4,12}".prop_map(|s| s)
    }

    /// Extract the address portion from a Via header value
    /// e.g. "SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK..." -> "192.168.1.1:5060"
    fn extract_via_addr(via_value: &str) -> &str {
        let after_udp = via_value
            .find("SIP/2.0/UDP ")
            .map(|i| &via_value[i + 12..])
            .unwrap_or(via_value);
        after_udp.split(';').next().unwrap_or(after_udp).trim()
    }

    // ===== Bug condition exploration property tests (Property 1: Fault Condition) =====
    // **Validates: Requirements 1.1, 1.2, 2.1, 2.2**
    //
    // These tests assert that Via headers contain local_addr (the expected behavior).
    // On unfixed code they will FAIL, proving the bug exists: Via contains proxy_addr instead.

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn pbt_build_invite_request_via_contains_local_addr(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
        ) {
            // Arrange: create Uac with proxy_addr in config
            let config = UacConfig {
                proxy_addr,
                local_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
            };
            let transport = Arc::new(MockTransport::new());
            let uac = Uac::new(
                transport,
                Arc::new(DialogManager::new(100)),
                Arc::new(StatsCollector::new()),
                make_user_pool(),
                config,
            );
            let dialog = Dialog {
                call_id: call_id.clone(),
                from_tag,
                to_tag: None,
                state: DialogState::Trying,
                cseq: 1,
                created_at: Instant::now(),
                session_expires: None,
                auth_attempted: false,
                original_request: None,
            };
            let user = &UserEntry {
                username: "alice".to_string(),
                domain: "example.com".to_string(),
                password: "secret".to_string(),
            };

            // Act
            let request = uac.build_invite_request(&dialog, user);

            // Assert: Via header should contain local_addr, not proxy_addr
            let via = request.headers.get("Via").expect("Via header must exist");
            let via_addr_str = extract_via_addr(&via);
            let expected = local_addr.to_string();
            prop_assert!(
                via_addr_str == expected,
                "INVITE Via header should contain local_addr ({}), but got {}",
                expected,
                via_addr_str
            );
        }

        #[test]
        fn pbt_build_register_request_via_contains_local_addr(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
        ) {
            let config = UacConfig {
                proxy_addr,
                local_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
            };
            let transport = Arc::new(MockTransport::new());
            let uac = Uac::new(
                transport,
                Arc::new(DialogManager::new(100)),
                Arc::new(StatsCollector::new()),
                make_user_pool(),
                config,
            );
            let dialog = Dialog {
                call_id: call_id.clone(),
                from_tag,
                to_tag: None,
                state: DialogState::Trying,
                cseq: 1,
                created_at: Instant::now(),
                session_expires: None,
                auth_attempted: false,
                original_request: None,
            };
            let user = &UserEntry {
                username: "alice".to_string(),
                domain: "example.com".to_string(),
                password: "secret".to_string(),
            };

            let request = uac.build_register_request(&dialog, user);

            let via = request.headers.get("Via").expect("Via header must exist");
            let via_addr_str = extract_via_addr(&via);
            let expected = local_addr.to_string();
            prop_assert!(
                via_addr_str == expected,
                "REGISTER Via header should contain local_addr ({}), but got {}",
                expected,
                via_addr_str
            );
        }

        #[test]
        fn pbt_build_ack_request_via_contains_local_addr(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
        ) {
            let config = UacConfig {
                proxy_addr,
                local_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
            };
            let transport = Arc::new(MockTransport::new());
            let uac = Uac::new(
                transport,
                Arc::new(DialogManager::new(100)),
                Arc::new(StatsCollector::new()),
                make_user_pool(),
                config,
            );

            let request = uac.build_ack_request(&call_id, &from_tag, None, 1);

            let via = request.headers.get("Via").expect("Via header must exist");
            let via_addr_str = extract_via_addr(&via);
            let expected = local_addr.to_string();
            prop_assert!(
                via_addr_str == expected,
                "ACK Via header should contain local_addr ({}), but got {}",
                expected,
                via_addr_str
            );
        }

        #[test]
        fn pbt_build_bye_request_static_via_contains_local_addr(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
        ) {
            // After fix: build_bye_request_static receives local_addr and uses it in Via.
            let request = build_bye_request_static(&call_id, &from_tag, None, 1, local_addr);

            let via = request.headers.get("Via").expect("Via header must exist");
            let via_addr_str = extract_via_addr(&via);
            let expected = local_addr.to_string();
            let not_expected = proxy_addr.to_string();
            // Via should contain local_addr, not proxy_addr
            prop_assert!(
                via_addr_str == expected,
                "BYE Via header should contain local_addr ({}), but got {}",
                expected,
                via_addr_str
            );
            prop_assert!(
                via_addr_str != not_expected,
                "BYE Via header should not contain proxy_addr ({}), but it does",
                not_expected
            );
        }

        #[test]
        fn pbt_build_reinvite_request_static_via_contains_local_addr(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
        ) {
            // After fix: build_reinvite_request_static receives local_addr and uses it in Via.
            let request = build_reinvite_request_static(&call_id, &from_tag, None, 1, local_addr);

            let via = request.headers.get("Via").expect("Via header must exist");
            let via_addr_str = extract_via_addr(&via);
            let expected = local_addr.to_string();
            let not_expected = proxy_addr.to_string();
            prop_assert!(
                via_addr_str == expected,
                "re-INVITE Via header should contain local_addr ({}), but got {}",
                expected,
                via_addr_str
            );
            prop_assert!(
                via_addr_str != not_expected,
                "re-INVITE Via header should not contain proxy_addr ({}), but it does",
                not_expected
            );
        }
    }

    // ===== Preservation property tests (Property 2: Preservation) =====
    // **Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7**
    //
    // These tests verify that non-Via headers and request structure are preserved.
    // They capture the baseline behavior of the unfixed code and must continue
    // to pass after the Via header fix is applied.

    /// Generate an arbitrary username for proptest
    fn arb_username() -> impl Strategy<Value = String> {
        "[a-z]{3,8}".prop_map(|s| s)
    }

    /// Generate an arbitrary domain for proptest
    fn arb_domain() -> impl Strategy<Value = String> {
        "[a-z]{3,8}\\.[a-z]{2,4}".prop_map(|s| s)
    }

    /// Generate an arbitrary cseq number
    fn arb_cseq() -> impl Strategy<Value = u32> {
        1u32..10000
    }

    /// Extract the branch value from a Via header
    /// e.g. "SIP/2.0/UDP 1.2.3.4:5060;branch=z9hG4bKabcdef0123456789" -> "z9hG4bKabcdef0123456789"
    fn extract_via_branch(via_value: &str) -> &str {
        via_value
            .find("branch=")
            .map(|i| {
                let start = i + 7;
                let rest = &via_value[start..];
                rest.split(';').next().unwrap_or(rest).trim()
            })
            .unwrap_or("")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Preservation: build_invite_request produces correct non-Via headers and request structure
        #[test]
        fn pbt_invite_preserves_headers_and_structure(
            proxy_addr in arb_socket_addr(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
            username in arb_username(),
            domain in arb_domain(),
            cseq in arb_cseq(),
        ) {
            let config = UacConfig {
                proxy_addr,
                local_addr: proxy_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
            };
            let transport = Arc::new(MockTransport::new());
            let uac = Uac::new(
                transport,
                Arc::new(DialogManager::new(100)),
                Arc::new(StatsCollector::new()),
                make_user_pool(),
                config,
            );
            let dialog = Dialog {
                call_id: call_id.clone(),
                from_tag: from_tag.clone(),
                to_tag: None,
                state: DialogState::Trying,
                cseq,
                created_at: Instant::now(),
                session_expires: None,
                auth_attempted: false,
                original_request: None,
            };
            let user = UserEntry {
                username: username.clone(),
                domain: domain.clone(),
                password: "secret".to_string(),
            };

            let request = uac.build_invite_request(&dialog, &user);

            // From header
            let from = request.headers.get("From").expect("From header must exist");
            let expected_from = format!("<sip:{}@{}>;tag={}", username, domain, from_tag);
            prop_assert_eq!(&from, &expected_from, "From header mismatch");

            // To header
            let to = request.headers.get("To").expect("To header must exist");
            let expected_to = format!("<sip:{}@{}>", username, domain);
            prop_assert_eq!(&to, &expected_to, "To header mismatch");

            // Call-ID
            let cid = request.headers.get("Call-ID").expect("Call-ID header must exist");
            prop_assert_eq!(&cid, &call_id, "Call-ID mismatch");

            // CSeq (INVITE)
            let cseq_hdr = request.headers.get("CSeq").expect("CSeq header must exist");
            let expected_cseq = format!("{} INVITE", cseq);
            prop_assert_eq!(&cseq_hdr, &expected_cseq, "CSeq mismatch");

            // Max-Forwards
            let mf = request.headers.get("Max-Forwards").expect("Max-Forwards header must exist");
            prop_assert_eq!(mf, "70".to_string(), "Max-Forwards mismatch");

            // Content-Length
            let cl = request.headers.get("Content-Length").expect("Content-Length header must exist");
            prop_assert_eq!(cl, "0".to_string(), "Content-Length mismatch");

            // Request URI
            let expected_uri = format!("sip:{}@{}", username, domain);
            prop_assert_eq!(&request.request_uri, &expected_uri, "Request URI mismatch");

            // Via branch value: must start with z9hG4bK magic cookie
            let via = request.headers.get("Via").expect("Via header must exist");
            let branch = extract_via_branch(&via);
            prop_assert!(
                branch.starts_with("z9hG4bK"),
                "Via branch must start with z9hG4bK magic cookie, got: {}",
                branch
            );
        }

        /// Preservation: build_register_request produces correct non-Via headers and request structure
        #[test]
        fn pbt_register_preserves_headers_and_structure(
            proxy_addr in arb_socket_addr(),
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
            username in arb_username(),
            domain in arb_domain(),
            cseq in arb_cseq(),
        ) {
            let config = UacConfig {
                proxy_addr,
                local_addr: proxy_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
            };
            let transport = Arc::new(MockTransport::new());
            let uac = Uac::new(
                transport,
                Arc::new(DialogManager::new(100)),
                Arc::new(StatsCollector::new()),
                make_user_pool(),
                config,
            );
            let dialog = Dialog {
                call_id: call_id.clone(),
                from_tag: from_tag.clone(),
                to_tag: None,
                state: DialogState::Trying,
                cseq,
                created_at: Instant::now(),
                session_expires: None,
                auth_attempted: false,
                original_request: None,
            };
            let user = UserEntry {
                username: username.clone(),
                domain: domain.clone(),
                password: "secret".to_string(),
            };

            let request = uac.build_register_request(&dialog, &user);

            // From header
            let from = request.headers.get("From").expect("From header must exist");
            let expected_from = format!("<sip:{}@{}>;tag={}", username, domain, from_tag);
            prop_assert_eq!(&from, &expected_from, "From header mismatch");

            // To header
            let to = request.headers.get("To").expect("To header must exist");
            let expected_to = format!("<sip:{}@{}>", username, domain);
            prop_assert_eq!(&to, &expected_to, "To header mismatch");

            // Call-ID
            let cid = request.headers.get("Call-ID").expect("Call-ID header must exist");
            prop_assert_eq!(&cid, &call_id, "Call-ID mismatch");

            // CSeq (REGISTER)
            let cseq_hdr = request.headers.get("CSeq").expect("CSeq header must exist");
            let expected_cseq = format!("{} REGISTER", cseq);
            prop_assert_eq!(&cseq_hdr, &expected_cseq, "CSeq mismatch");

            // Max-Forwards
            let mf = request.headers.get("Max-Forwards").expect("Max-Forwards header must exist");
            prop_assert_eq!(mf, "70".to_string(), "Max-Forwards mismatch");

            // Content-Length
            let cl = request.headers.get("Content-Length").expect("Content-Length header must exist");
            prop_assert_eq!(cl, "0".to_string(), "Content-Length mismatch");

            // Request URI: sip:domain (not sip:user@domain)
            let expected_uri = format!("sip:{}", domain);
            prop_assert_eq!(&request.request_uri, &expected_uri, "Request URI mismatch");

            // Via branch value: must start with z9hG4bK magic cookie
            let via = request.headers.get("Via").expect("Via header must exist");
            let branch = extract_via_branch(&via);
            prop_assert!(
                branch.starts_with("z9hG4bK"),
                "Via branch must start with z9hG4bK magic cookie, got: {}",
                branch
            );
        }
    }

    // ===== Bug condition exploration property tests (Property 3: Branch Uniqueness) =====
    // **Validates: Requirements 1.5, 2.5**
    //
    // These tests assert that different transactions within the same dialog produce
    // different branch values. On unfixed code they will FAIL, proving the branch
    // collision bug exists: branch is derived solely from Call-ID, ignoring CSeq/method.

    /// Generate a pair of distinct CSeq numbers for proptest
    fn arb_distinct_cseq_pair() -> impl Strategy<Value = (u32, u32)> {
        (arb_cseq(), arb_cseq())
            .prop_filter("cseq1 must differ from cseq2", |(a, b)| a != b)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        /// Bug condition exploration: different transactions (INVITE vs BYE) within the
        /// same dialog must produce different branch values.
        /// On unfixed code this FAILS because branch = z9hG4bK + call_id[..16],
        /// which is identical for all transactions sharing the same Call-ID.
        #[test]
        fn pbt_branch_uniqueness_invite_vs_bye(
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
            local_addr in arb_socket_addr(),
            (cseq1, cseq2) in arb_distinct_cseq_pair(),
        ) {
            // Build an INVITE request
            let invite = build_reinvite_request_static(
                &call_id, &from_tag, Some("remote-tag"), cseq1, local_addr,
            );
            // Build a BYE request with the same Call-ID but different CSeq
            let bye = build_bye_request_static(
                &call_id, &from_tag, Some("remote-tag"), cseq2, local_addr,
            );

            let invite_via = invite.headers.get("Via").expect("INVITE Via header must exist");
            let bye_via = bye.headers.get("Via").expect("BYE Via header must exist");

            let invite_branch = extract_via_branch(&invite_via);
            let bye_branch = extract_via_branch(&bye_via);

            // Different transactions MUST have different branch values (RFC 3261)
            prop_assert_ne!(
                invite_branch, bye_branch,
                "Branch collision: INVITE (CSeq {}) and BYE (CSeq {}) produced the same branch '{}' for Call-ID '{}'",
                cseq1, cseq2, invite_branch, call_id
            );
        }
    }

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

        fn sent_requests(&self) -> Vec<SipRequest> {
            self.sent_messages()
                .into_iter()
                .filter_map(|(msg, _)| match msg {
                    SipMessage::Request(r) => Some(r),
                    _ => None,
                })
                .collect()
        }

        fn sent_count(&self) -> usize {
            self.sent.lock().unwrap().len()
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

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5060".parse().unwrap()
    }

    fn make_user_pool() -> Arc<UserPool> {
        let users_file = UsersFile {
            users: vec![
                UserEntry {
                    username: "alice".to_string(),
                    domain: "example.com".to_string(),
                    password: "secret1".to_string(),
                },
                UserEntry {
                    username: "bob".to_string(),
                    domain: "example.com".to_string(),
                    password: "secret2".to_string(),
                },
            ],
        };
        Arc::new(UserPool::from_users_file(users_file).unwrap())
    }

    fn make_uac(transport: Arc<MockTransport>) -> Uac {
        Uac::new(
            transport,
            Arc::new(DialogManager::new(100)),
            Arc::new(StatsCollector::new()),
            make_user_pool(),
            UacConfig::default(),
        )
    }

    fn make_uac_with_parts(
        transport: Arc<MockTransport>,
    ) -> (Uac, Arc<DialogManager>, Arc<StatsCollector>) {
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let uac = Uac::new(
            transport,
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );
        (uac, dm, stats)
    }

    fn make_response(status_code: u16, reason: &str, call_id: &str, cseq: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", cseq.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=def".to_string());
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code,
            reason_phrase: reason.to_string(),
            headers,
            body: None,
        })
    }

    // ===== UacConfig tests =====

    #[test]
    fn test_uac_config_default() {
        let config = UacConfig::default();
        assert_eq!(config.proxy_addr, test_addr());
        assert_eq!(config.local_addr, test_addr());
        assert_eq!(config.call_duration, Duration::from_secs(3));
        assert_eq!(config.dialog_timeout, Duration::from_secs(32));
    }

    #[test]
    fn test_uac_config_custom() {
        let config = UacConfig {
            proxy_addr: "10.0.0.1:5080".parse().unwrap(),
            local_addr: "10.0.0.2:5070".parse().unwrap(),
            call_duration: Duration::from_secs(10),
            dialog_timeout: Duration::from_secs(60),
        };
        assert_eq!(config.proxy_addr.port(), 5080);
        assert_eq!(config.local_addr.port(), 5070);
        assert_eq!(config.call_duration, Duration::from_secs(10));
    }

    // ===== Uac construction tests =====

    #[test]
    fn test_uac_new_creates_instance() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        // Just verify it constructs without panic
        assert_eq!(uac.config.proxy_addr, test_addr());
    }

    // ===== send_register tests =====

    #[tokio::test]
    async fn test_send_register_sends_request() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        assert_eq!(transport.sent_count(), 1);
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Register);
    }

    #[tokio::test]
    async fn test_send_register_uses_user_from_pool() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        let requests = transport.sent_requests();
        let from = requests[0].headers.get("From").unwrap();
        // First user is alice (round-robin)
        assert!(from.contains("alice@example.com"), "From should contain alice, got: {}", from);
    }

    #[tokio::test]
    async fn test_send_register_round_robin_user_selection() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();
        uac.send_register().await.unwrap();

        let requests = transport.sent_requests();
        let from1 = requests[0].headers.get("From").unwrap();
        let from2 = requests[1].headers.get("From").unwrap();
        assert!(from1.contains("alice"), "First should be alice");
        assert!(from2.contains("bob"), "Second should be bob");
    }

    #[tokio::test]
    async fn test_send_register_creates_dialog() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();

        assert_eq!(dm.active_count(), 1);
    }

    #[tokio::test]
    async fn test_send_register_dialog_in_trying_state() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();

        let call_ids = dm.all_call_ids();
        assert_eq!(call_ids.len(), 1);
        let dialog = dm.get_dialog(&call_ids[0]).unwrap();
        assert_eq!(dialog.state, DialogState::Trying);
    }

    #[tokio::test]
    async fn test_send_register_increments_active_dialogs_stat() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _, stats) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();

        assert_eq!(stats.snapshot().active_dialogs, 1);
    }

    #[tokio::test]
    async fn test_send_register_sends_to_proxy_addr() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        let sent = transport.sent_messages();
        assert_eq!(sent[0].1, test_addr());
    }

    #[tokio::test]
    async fn test_send_register_has_required_headers() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        let requests = transport.sent_requests();
        let req = &requests[0];
        assert!(req.headers.get("Via").is_some(), "Missing Via header");
        assert!(req.headers.get("From").is_some(), "Missing From header");
        assert!(req.headers.get("To").is_some(), "Missing To header");
        assert!(req.headers.get("Call-ID").is_some(), "Missing Call-ID header");
        assert!(req.headers.get("CSeq").is_some(), "Missing CSeq header");
    }

    #[tokio::test]
    async fn test_send_register_cseq_contains_register() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        let requests = transport.sent_requests();
        let cseq = requests[0].headers.get("CSeq").unwrap();
        assert!(cseq.contains("REGISTER"), "CSeq should contain REGISTER, got: {}", cseq);
    }

    #[tokio::test]
    async fn test_send_register_request_uri_is_domain() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_register().await.unwrap();

        let requests = transport.sent_requests();
        assert!(requests[0].request_uri.starts_with("sip:example.com"));
    }

    #[tokio::test]
    async fn test_send_register_stores_original_request() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();

        let call_ids = dm.all_call_ids();
        let dialog = dm.get_dialog(&call_ids[0]).unwrap();
        assert!(dialog.original_request.is_some(), "Should store original request");
    }

    // ===== send_invite tests =====

    #[tokio::test]
    async fn test_send_invite_sends_request() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_invite().await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Invite);
    }

    #[tokio::test]
    async fn test_send_invite_uses_user_from_pool() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_invite().await.unwrap();

        let requests = transport.sent_requests();
        let from = requests[0].headers.get("From").unwrap();
        assert!(from.contains("alice@example.com"));
    }

    #[tokio::test]
    async fn test_send_invite_creates_dialog_in_trying() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();

        let call_ids = dm.all_call_ids();
        assert_eq!(call_ids.len(), 1);
        let dialog = dm.get_dialog(&call_ids[0]).unwrap();
        assert_eq!(dialog.state, DialogState::Trying);
    }

    #[tokio::test]
    async fn test_send_invite_cseq_contains_invite() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_invite().await.unwrap();

        let requests = transport.sent_requests();
        let cseq = requests[0].headers.get("CSeq").unwrap();
        assert!(cseq.contains("INVITE"));
    }

    #[tokio::test]
    async fn test_send_invite_request_uri_contains_user() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        uac.send_invite().await.unwrap();

        let requests = transport.sent_requests();
        assert!(requests[0].request_uri.contains("alice@example.com"));
    }

    // ===== handle_response tests =====

    #[tokio::test]
    async fn test_handle_response_register_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        // Send REGISTER first to create dialog
        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // Simulate 200 OK response
        let response = make_response(200, "OK", &call_id, "1 REGISTER");
        uac.handle_response(response, test_addr()).await.unwrap();

        // Dialog should be removed (completed)
        assert_eq!(dm.active_count(), 0);
        // Success should be recorded
        let snap = stats.snapshot();
        assert_eq!(snap.successful_calls, 1);
    }

    #[tokio::test]
    async fn test_handle_response_register_200_decrements_active_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();
        assert_eq!(stats.snapshot().active_dialogs, 1);

        let call_ids = dm.all_call_ids();
        let response = make_response(200, "OK", &call_ids[0], "1 REGISTER");
        uac.handle_response(response, test_addr()).await.unwrap();

        assert_eq!(stats.snapshot().active_dialogs, 0);
    }

    #[tokio::test]
    async fn test_handle_response_invite_200_sends_ack() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // Clear sent messages from INVITE
        transport.sent.lock().unwrap().clear();

        let response = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        // Should have sent ACK
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1, "Should send exactly 1 ACK");
        assert_eq!(requests[0].method, Method::Ack);
    }

    #[tokio::test]
    async fn test_handle_response_invite_200_updates_dialog_to_confirmed() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        // Dialog should be in Confirmed state (briefly, before BYE timer fires)
        // Note: the spawned BYE task runs after call_duration, so immediately after
        // handle_response the dialog should be Confirmed
        let dialog = dm.get_dialog(&call_id).unwrap();
        assert_eq!(dialog.state, DialogState::Confirmed);
    }

    #[tokio::test]
    async fn test_handle_response_bye_200_removes_dialog() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        // Manually create a dialog in Confirmed state to simulate post-INVITE
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        stats.increment_active_dialogs();

        let response = make_response(200, "OK", &call_id, "2 BYE");
        uac.handle_response(response, test_addr()).await.unwrap();

        assert_eq!(dm.active_count(), 0);
        assert_eq!(stats.snapshot().successful_calls, 1);
    }

    #[tokio::test]
    async fn test_handle_response_unknown_call_id_ignored() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        let response = make_response(200, "OK", "unknown-call-id", "1 REGISTER");
        let result = uac.handle_response(response, test_addr()).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_response_ignores_request_messages() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        let request = SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let result = uac.handle_response(request, test_addr()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_handle_response_empty_call_id_ignored() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        let response = make_response(200, "OK", "", "1 REGISTER");
        let result = uac.handle_response(response, test_addr()).await;
        assert!(result.is_ok());
    }

    // ===== Error response handling tests =====

    #[tokio::test]
    async fn test_handle_response_404_records_failure() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(404, "Not Found", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.failed_calls, 1, "404 should record a failure");
    }

    #[tokio::test]
    async fn test_handle_response_404_removes_dialog() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(404, "Not Found", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        assert!(dm.get_dialog(&call_id).is_none(), "Dialog should be removed after 404");
    }

    #[tokio::test]
    async fn test_handle_response_404_decrements_active_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        assert_eq!(stats.snapshot().active_dialogs, 1);

        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(404, "Not Found", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        assert_eq!(stats.snapshot().active_dialogs, 0, "Active dialogs should decrement after 404");
    }

    #[tokio::test]
    async fn test_handle_response_500_records_failure() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(500, "Server Internal Error", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.failed_calls, 1, "500 should record a failure");
    }

    #[tokio::test]
    async fn test_handle_response_100_trying_does_not_record_failure() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(100, "Trying", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.failed_calls, 0, "100 Trying should NOT record a failure");
        assert!(dm.get_dialog(&call_id).is_some(), "Dialog should still exist after 100 Trying");
    }

    // ===== check_timeouts tests =====

    #[tokio::test]
    async fn test_check_timeouts_no_timeouts_when_fresh() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _, _) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();

        let timed_out = uac.check_timeouts();
        assert_eq!(timed_out, 0);
    }

    #[test]
    fn test_check_timeouts_removes_expired_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        // Create a dialog with a past creation time by using very short timeout
        let config = UacConfig {
            dialog_timeout: Duration::from_millis(0), // Immediate timeout
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport,
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        // Create a dialog manually
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Trying).unwrap();
        stats.increment_active_dialogs();

        // Small sleep to ensure timeout
        std::thread::sleep(Duration::from_millis(1));

        let timed_out = uac.check_timeouts();
        assert_eq!(timed_out, 1);
        assert_eq!(dm.active_count(), 0);
    }

    #[test]
    fn test_check_timeouts_records_failure() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            dialog_timeout: Duration::from_millis(0),
            ..UacConfig::default()
        };

        let uac = Uac::new(transport, dm.clone(), stats.clone(), make_user_pool(), config);

        let dialog = dm.create_dialog().unwrap();
        dm.update_dialog_state(&dialog.call_id, DialogState::Trying).unwrap();
        stats.increment_active_dialogs();

        std::thread::sleep(Duration::from_millis(1));

        uac.check_timeouts();

        let snap = stats.snapshot();
        assert_eq!(snap.failed_calls, 1);
        assert_eq!(snap.active_dialogs, 0);
    }

    #[test]
    fn test_check_timeouts_skips_terminated_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            dialog_timeout: Duration::from_millis(0),
            ..UacConfig::default()
        };

        let uac = Uac::new(transport, dm.clone(), stats.clone(), make_user_pool(), config);

        let dialog = dm.create_dialog().unwrap();
        dm.update_dialog_state(&dialog.call_id, DialogState::Terminated).unwrap();

        std::thread::sleep(Duration::from_millis(1));

        let timed_out = uac.check_timeouts();
        assert_eq!(timed_out, 0, "Terminated dialogs should not be timed out");
    }

    #[test]
    fn test_check_timeouts_skips_error_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            dialog_timeout: Duration::from_millis(0),
            ..UacConfig::default()
        };

        let uac = Uac::new(transport, dm.clone(), stats.clone(), make_user_pool(), config);

        let dialog = dm.create_dialog().unwrap();
        dm.update_dialog_state(&dialog.call_id, DialogState::Error).unwrap();

        std::thread::sleep(Duration::from_millis(1));

        let timed_out = uac.check_timeouts();
        assert_eq!(timed_out, 0, "Error dialogs should not be timed out");
    }

    // ===== shutdown tests =====

    #[tokio::test]
    async fn test_shutdown_sends_bye_for_confirmed_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );

        // Create a confirmed dialog
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        stats.increment_active_dialogs();

        uac.shutdown().await.unwrap();

        // Should have sent BYE
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Bye);
    }

    #[tokio::test]
    async fn test_shutdown_does_not_send_bye_for_trying_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );

        // Create a dialog in Trying state
        let dialog = dm.create_dialog().unwrap();
        dm.update_dialog_state(&dialog.call_id, DialogState::Trying).unwrap();
        stats.increment_active_dialogs();

        uac.shutdown().await.unwrap();

        // Should NOT have sent BYE (only for Confirmed)
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 0);
    }

    #[tokio::test]
    async fn test_shutdown_removes_all_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );

        // Create multiple dialogs in various states
        let d1 = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d1.call_id, DialogState::Confirmed).unwrap();
        stats.increment_active_dialogs();

        let d2 = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d2.call_id, DialogState::Trying).unwrap();
        stats.increment_active_dialogs();

        uac.shutdown().await.unwrap();

        assert_eq!(dm.active_count(), 0);
    }

    // ===== Call-ID based dispatch tests =====

    #[tokio::test]
    async fn test_response_dispatched_to_correct_dialog() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());

        // Create two REGISTER dialogs
        uac.send_register().await.unwrap();
        uac.send_register().await.unwrap();

        let call_ids = dm.all_call_ids();
        assert_eq!(call_ids.len(), 2);

        // Send 200 OK for the first dialog only
        let response = make_response(200, "OK", &call_ids[0], "1 REGISTER");
        uac.handle_response(response, test_addr()).await.unwrap();

        // First dialog should be removed, second should remain
        assert!(dm.get_dialog(&call_ids[0]).is_none(), "First dialog should be removed");
        assert!(dm.get_dialog(&call_ids[1]).is_some(), "Second dialog should remain");
        assert_eq!(dm.active_count(), 1);
    }

    // ===== Full REGISTER flow test =====

    #[tokio::test]
    async fn test_full_register_flow() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        // Send REGISTER
        uac.send_register().await.unwrap();
        assert_eq!(dm.active_count(), 1);
        assert_eq!(stats.snapshot().active_dialogs, 1);

        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // Receive 200 OK
        let response = make_response(200, "OK", &call_id, "1 REGISTER");
        uac.handle_response(response, test_addr()).await.unwrap();

        // Dialog completed
        assert_eq!(dm.active_count(), 0);
        assert_eq!(stats.snapshot().active_dialogs, 0);
        assert_eq!(stats.snapshot().successful_calls, 1);
        assert_eq!(stats.snapshot().total_calls, 1);
    }

    // ===== Full INVITE-ACK-BYE flow test (without waiting for timer) =====

    #[tokio::test]
    async fn test_invite_200_ok_sends_ack_with_correct_call_id() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let response = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests[0].method, Method::Ack);
        assert_eq!(requests[0].headers.get("Call-ID").unwrap(), call_id);
    }

    // ===== Multiple concurrent dialogs =====

    #[tokio::test]
    async fn test_multiple_concurrent_registers() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        for _ in 0..5 {
            uac.send_register().await.unwrap();
        }

        assert_eq!(dm.active_count(), 5);
        assert_eq!(stats.snapshot().active_dialogs, 5);
        assert_eq!(transport.sent_count(), 5);
    }

    // ===== Session-Timer tests (Req 3.1) =====

    /// Helper: create a 200 OK response with Session-Expires header
    fn make_response_with_session_expires(
        call_id: &str,
        cseq: &str,
        session_expires: u32,
    ) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", cseq.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:bob@example.com>;tag=def".to_string());
        headers.set("Session-Expires", format!("{}", session_expires));
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        })
    }

    #[tokio::test]
    async fn test_invite_200_with_session_expires_records_in_dialog() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response_with_session_expires(&call_id, "1 INVITE", 1800);
        uac.handle_response(response, test_addr()).await.unwrap();

        let dialog = dm.get_dialog(&call_id).unwrap();
        assert_eq!(
            dialog.session_expires,
            Some(Duration::from_secs(1800)),
            "Dialog should record session_expires from 200 OK"
        );
    }

    #[tokio::test]
    async fn test_invite_200_without_session_expires_dialog_has_none() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _) = make_uac_with_parts(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        let response = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        let dialog = dm.get_dialog(&call_id).unwrap();
        assert_eq!(
            dialog.session_expires, None,
            "Dialog should have no session_expires when 200 OK doesn't have it"
        );
    }

    #[tokio::test]
    async fn test_session_timer_sends_reinvite_at_half_expiry() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        // Use very short session_expires (200ms) so re-INVITE fires at 100ms
        // and call_duration long enough (2s) that BYE doesn't fire first
        let config = UacConfig {
            call_duration: Duration::from_secs(2),
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        // 200 OK with Session-Expires: 1 (1 second, so re-INVITE at 500ms)
        let response = make_response_with_session_expires(&call_id, "1 INVITE", 1);
        uac.handle_response(response, test_addr()).await.unwrap();

        // ACK should be sent immediately
        let requests_after_ack = transport.sent_requests();
        assert_eq!(requests_after_ack.len(), 1);
        assert_eq!(requests_after_ack[0].method, Method::Ack);

        // Wait for re-INVITE to fire (session_expires/2 = 500ms, give some margin)
        tokio::time::sleep(Duration::from_millis(700)).await;

        let all_requests = transport.sent_requests();
        // Should have ACK + re-INVITE
        let reinvites: Vec<_> = all_requests.iter().filter(|r| r.method == Method::Invite).collect();
        assert_eq!(reinvites.len(), 1, "Should send exactly 1 re-INVITE");
    }

    #[tokio::test]
    async fn test_reinvite_has_same_call_id() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            call_duration: Duration::from_secs(2),
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let response = make_response_with_session_expires(&call_id, "1 INVITE", 1);
        uac.handle_response(response, test_addr()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;

        let all_requests = transport.sent_requests();
        let reinvites: Vec<_> = all_requests.iter().filter(|r| r.method == Method::Invite).collect();
        assert_eq!(reinvites.len(), 1);
        assert_eq!(
            reinvites[0].headers.get("Call-ID").unwrap(),
            call_id,
            "re-INVITE should have the same Call-ID as the original dialog"
        );
    }

    #[tokio::test]
    async fn test_reinvite_has_incremented_cseq() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            call_duration: Duration::from_secs(2),
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let response = make_response_with_session_expires(&call_id, "1 INVITE", 1);
        uac.handle_response(response, test_addr()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;

        let all_requests = transport.sent_requests();
        let reinvites: Vec<_> = all_requests.iter().filter(|r| r.method == Method::Invite).collect();
        assert_eq!(reinvites.len(), 1);

        let cseq = reinvites[0].headers.get("CSeq").unwrap();
        assert!(
            cseq.contains("INVITE"),
            "re-INVITE CSeq should contain INVITE method, got: {}", cseq
        );
        // Original CSeq was 1, so re-INVITE should be 2 INVITE
        assert!(
            cseq.starts_with("2"),
            "re-INVITE CSeq should be incremented (expected 2), got: {}", cseq
        );
    }

    #[tokio::test]
    async fn test_no_reinvite_when_no_session_expires() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            call_duration: Duration::from_secs(2),
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        // 200 OK without Session-Expires
        let response = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(response, test_addr()).await.unwrap();

        // Wait a bit to ensure no re-INVITE fires
        tokio::time::sleep(Duration::from_millis(200)).await;

        let all_requests = transport.sent_requests();
        let reinvites: Vec<_> = all_requests.iter().filter(|r| r.method == Method::Invite).collect();
        assert_eq!(
            reinvites.len(), 0,
            "Should NOT send re-INVITE when no Session-Expires in 200 OK"
        );
    }

    #[tokio::test]
    async fn test_reinvite_has_from_and_to_tags() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        let config = UacConfig {
            call_duration: Duration::from_secs(2),
            ..UacConfig::default()
        };

        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let response = make_response_with_session_expires(&call_id, "1 INVITE", 1);
        uac.handle_response(response, test_addr()).await.unwrap();

        tokio::time::sleep(Duration::from_millis(700)).await;

        let all_requests = transport.sent_requests();
        let reinvites: Vec<_> = all_requests.iter().filter(|r| r.method == Method::Invite).collect();
        assert_eq!(reinvites.len(), 1);

        let from = reinvites[0].headers.get("From").unwrap();
        let to = reinvites[0].headers.get("To").unwrap();
        assert!(from.contains("tag="), "re-INVITE From should have a tag");
        assert!(to.contains("tag="), "re-INVITE To should have a tag");
    }

    // ===== Digest Auth Integration Tests =====

    use crate::auth::DigestAuth;

    /// Helper: create a UAC with DigestAuth enabled
    fn make_uac_with_auth(
        transport: Arc<MockTransport>,
    ) -> (Uac, Arc<DialogManager>, Arc<StatsCollector>) {
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let user_pool = make_user_pool();
        let auth = DigestAuth::new(user_pool.clone());
        let uac = Uac::new(
            transport,
            dm.clone(),
            stats.clone(),
            user_pool,
            UacConfig::default(),
        );
        // Set auth on the UAC
        let mut uac = uac;
        uac.auth = Some(auth);
        (uac, dm, stats)
    }

    /// Helper: create a 401 Unauthorized response with WWW-Authenticate header
    fn make_401_response(call_id: &str, cseq: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", cseq.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:alice@example.com>;tag=def".to_string());
        headers.set(
            "WWW-Authenticate",
            "Digest realm=\"example.com\", nonce=\"abc123\", algorithm=MD5".to_string(),
        );
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 401,
            reason_phrase: "Unauthorized".to_string(),
            headers,
            body: None,
        })
    }

    /// Helper: create a 407 Proxy Authentication Required response
    fn make_407_response(call_id: &str, cseq: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK123".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", cseq.to_string());
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:alice@example.com>;tag=def".to_string());
        headers.set(
            "Proxy-Authenticate",
            "Digest realm=\"example.com\", nonce=\"xyz789\", algorithm=MD5".to_string(),
        );
        SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 407,
            reason_phrase: "Proxy Authentication Required".to_string(),
            headers,
            body: None,
        })
    }

    #[tokio::test]
    async fn test_handle_401_resends_register_with_authorization() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        // Send initial REGISTER
        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // Clear sent messages to isolate the re-send
        transport.sent.lock().unwrap().clear();

        // Receive 401 response
        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // Should have re-sent the REGISTER with Authorization header
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1, "Should re-send one request after 401");
        assert_eq!(requests[0].method, Method::Register);
        let auth_header = requests[0].headers.get("Authorization");
        assert!(auth_header.is_some(), "Re-sent request should have Authorization header");
        assert!(auth_header.unwrap().starts_with("Digest "), "Should be Digest auth");
    }

    #[tokio::test]
    async fn test_handle_407_resends_invite_with_proxy_authorization() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        // Send initial INVITE
        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        // Receive 407 response
        let resp = make_407_response(&call_id, "1 INVITE");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // Should have re-sent the INVITE with Proxy-Authorization header
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1, "Should re-send one request after 407");
        assert_eq!(requests[0].method, Method::Invite);
        let proxy_auth_header = requests[0].headers.get("Proxy-Authorization");
        assert!(proxy_auth_header.is_some(), "Re-sent request should have Proxy-Authorization header");
        assert!(proxy_auth_header.unwrap().starts_with("Digest "), "Should be Digest auth");
    }

    #[tokio::test]
    async fn test_handle_401_increments_cseq() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        let cseq = requests[0].headers.get("CSeq").unwrap();
        assert!(cseq.starts_with("2 "), "CSeq should be incremented to 2, got: {}", cseq);
    }

    #[tokio::test]
    async fn test_handle_407_increments_cseq() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let resp = make_407_response(&call_id, "1 INVITE");
        uac.handle_response(resp, test_addr()).await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        let cseq = requests[0].headers.get("CSeq").unwrap();
        assert!(cseq.starts_with("2 "), "CSeq should be incremented to 2, got: {}", cseq);
    }

    #[tokio::test]
    async fn test_handle_401_sets_auth_attempted_flag() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // Before auth: auth_attempted should be false
        {
            let dialog = dm.get_dialog(&call_id).unwrap();
            assert!(!dialog.auth_attempted, "auth_attempted should be false before 401");
        }

        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // After auth: auth_attempted should be true
        {
            let dialog = dm.get_dialog(&call_id).unwrap();
            assert!(dialog.auth_attempted, "auth_attempted should be true after 401 handling");
        }
    }

    #[tokio::test]
    async fn test_handle_401_after_auth_attempted_records_failure_no_resend() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_auth(transport.clone());

        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // First 401 → should re-send
        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        transport.sent.lock().unwrap().clear();

        // Second 401 → should NOT re-send (infinite loop prevention)
        let resp2 = make_401_response(&call_id, "2 REGISTER");
        uac.handle_response(resp2, test_addr()).await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 0, "Should NOT re-send after second 401 (infinite loop prevention)");

        // Should record auth failure
        let snap = stats.snapshot();
        assert!(snap.auth_failures > 0, "Should record auth failure");
    }

    #[tokio::test]
    async fn test_handle_407_after_auth_attempted_records_failure_no_resend() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_auth(transport.clone());

        uac.send_invite().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // First 407 → should re-send
        let resp = make_407_response(&call_id, "1 INVITE");
        uac.handle_response(resp, test_addr()).await.unwrap();

        transport.sent.lock().unwrap().clear();

        // Second 407 → should NOT re-send
        let resp2 = make_407_response(&call_id, "2 INVITE");
        uac.handle_response(resp2, test_addr()).await.unwrap();

        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 0, "Should NOT re-send after second 407");

        let snap = stats.snapshot();
        assert!(snap.auth_failures > 0, "Should record auth failure");
    }

    #[tokio::test]
    async fn test_handle_401_without_auth_module_records_failure() {
        // UAC without auth (auth = None) should treat 401 as failure
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_parts(transport.clone());

        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        transport.sent.lock().unwrap().clear();

        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // Should NOT re-send (no auth module)
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 0, "Should not re-send without auth module");

        // Should record failure
        let snap = stats.snapshot();
        assert!(snap.auth_failures > 0, "Should record auth failure when no auth module");
    }

    #[tokio::test]
    async fn test_handle_401_dialog_transitions_to_error_on_second_attempt() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_auth(transport.clone());

        uac.send_register().await.unwrap();
        let call_ids = dm.all_call_ids();
        let call_id = call_ids[0].clone();

        // First 401
        let resp = make_401_response(&call_id, "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // Second 401 → dialog should transition to Error
        let resp2 = make_401_response(&call_id, "2 REGISTER");
        uac.handle_response(resp2, test_addr()).await.unwrap();

        let dialog = dm.get_dialog(&call_id).unwrap();
        assert_eq!(dialog.state, DialogState::Error, "Dialog should be in Error state after repeated auth failure");
    }

    // ===== bg_register tests =====

    #[tokio::test]
    async fn test_bg_register_count_zero_returns_empty_result() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        let result = uac.bg_register(0, Duration::from_secs(5)).await;

        assert_eq!(result, BgRegisterResult { success: 0, failed: 0 });
        assert_eq!(transport.sent_count(), 0, "No messages should be sent when count is 0");
    }

    #[tokio::test]
    async fn test_bg_register_sends_correct_number_of_registers() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport.clone());

        // bg_register sends 3 REGISTERs, but no responses come → will timeout
        let result = uac.bg_register(3, Duration::from_millis(100)).await;

        assert_eq!(transport.sent_count(), 3, "Should send exactly 3 REGISTER requests");
        let requests = transport.sent_requests();
        for req in &requests {
            assert_eq!(req.method, Method::Register, "All sent messages should be REGISTER");
        }
    }

    #[tokio::test]
    async fn test_bg_register_returns_success_count_after_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());

        // Send 2 REGISTERs
        // We need to manually trigger bg_register and simulate responses
        // Since bg_register is async and waits, we need to spawn it and feed responses

        let uac = Arc::new(uac);
        let uac_clone = uac.clone();
        let _dm_clone = dm.clone();

        let handle = tokio::spawn(async move {
            uac_clone.bg_register(2, Duration::from_secs(2)).await
        });

        // Give time for REGISTERs to be sent
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Get the call IDs and simulate 200 OK responses
        let call_ids = dm.all_call_ids();
        assert_eq!(call_ids.len(), 2, "Should have 2 active dialogs");

        for call_id in &call_ids {
            let resp = make_response(200, "OK", call_id, "1 REGISTER");
            uac.handle_response(resp, test_addr()).await.unwrap();
        }

        let result = handle.await.unwrap();
        assert_eq!(result.success, 2);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn test_bg_register_handles_timeout_for_unresponsive_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());

        // Send 2 REGISTERs but only respond to 1
        let uac = Arc::new(uac);
        let uac_clone = uac.clone();

        let handle = tokio::spawn(async move {
            uac_clone.bg_register(2, Duration::from_millis(200)).await
        });

        // Give time for REGISTERs to be sent
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Respond to only the first dialog
        let call_ids = dm.all_call_ids();
        assert!(call_ids.len() >= 1);

        let resp = make_response(200, "OK", &call_ids[0], "1 REGISTER");
        uac.handle_response(resp, test_addr()).await.unwrap();

        let result = handle.await.unwrap();
        // 1 success (got 200 OK), 1 failed (timed out)
        assert_eq!(result.success, 1, "One REGISTER should succeed");
        assert_eq!(result.failed, 1, "One REGISTER should fail due to timeout");
    }

    #[tokio::test]
    async fn test_bg_register_all_timeout_reports_all_failed() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_parts(transport.clone());

        // Send 3 REGISTERs, respond to none → all should timeout
        let result = uac.bg_register(3, Duration::from_millis(100)).await;

        assert_eq!(result.success, 0, "No REGISTER should succeed");
        assert_eq!(result.failed, 3, "All REGISTERs should fail due to timeout");
    }
}
