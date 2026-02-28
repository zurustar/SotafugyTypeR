// UAC (User Agent Client) module
//
// Handles outgoing SIP requests (REGISTER, INVITE-BYE scenarios).
// Uses SipTransport trait for testability, UserPool for user selection,
// DialogManager for dialog tracking, and StatsCollector for statistics.

mod builders;
mod config;
pub mod load_pattern;

pub use config::*;
use builders::*;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::DigestAuth;
use crate::dialog::{Dialog, DialogManager, DialogState};
use crate::error::SipLoadTestError;
use crate::sip::formatter::format_sip_message;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
use crate::stats::StatsCollector;
use crate::transport::SipTransport;
use crate::user_pool::{UserEntry, UserPool};

/// UAC (User Agent Client) - generates and sends SIP requests.
pub struct Uac {
    transport: Arc<dyn SipTransport>,
    dialog_manager: Arc<DialogManager>,
    stats: Arc<StatsCollector>,
    pub auth: Option<DigestAuth>,
    user_pool: Arc<UserPool>,
    config: UacConfig,
    transaction_manager: Option<crate::transaction::TransactionManager>,
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
            transaction_manager: None,
        }
    }

    /// Create a new UAC instance with TransactionManager for retransmission control.
    pub fn with_transaction_manager(
        transport: Arc<dyn SipTransport>,
        dialog_manager: Arc<DialogManager>,
        stats: Arc<StatsCollector>,
        user_pool: Arc<UserPool>,
        config: UacConfig,
        transaction_manager: crate::transaction::TransactionManager,
    ) -> Self {
        Self {
            transport,
            dialog_manager,
            stats,
            auth: None,
            user_pool,
            config,
            transaction_manager: Some(transaction_manager),
        }
    }

    /// Get a reference to the dialog manager.
    pub fn dialog_manager(&self) -> &Arc<DialogManager> {
        &self.dialog_manager
    }

    /// Send a REGISTER request. Selects the next user from UserPool via round-robin.
    pub async fn send_register(&self) -> Result<(), SipLoadTestError> {
        let user = self.user_pool.next_user();
        let dialog = match self.dialog_manager.create_dialog() {
            Ok(d) => d,
            Err(SipLoadTestError::MaxDialogsReached(_)) => {
                self.stats.record_failure();
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let request = self.build_register_request(&dialog, user);

        // Store original request in dialog for potential re-send (auth)
        if let Some(mut d) = self.dialog_manager.get_dialog_mut(&dialog.call_id) {
            d.state = DialogState::Trying;
            d.original_request = Some(SipMessage::Request(request.clone()));
        }

        self.stats.increment_active_dialogs();

        // TransactionManager経由で送信（再送制御あり）、なければ直接送信
        if let Some(ref tm) = self.transaction_manager {
            tm.send_request(request, self.config.proxy_addr).await?;
        } else {
            let bytes = format_sip_message(&SipMessage::Request(request));
            self.transport.send_to(&bytes, self.config.proxy_addr).await?;
        }

        Ok(())
    }

    /// Send an INVITE request. Selects the next user from UserPool via round-robin.
    pub async fn send_invite(&self) -> Result<(), SipLoadTestError> {
        let user = self.user_pool.next_user();
        let dialog = match self.dialog_manager.create_dialog() {
            Ok(d) => d,
            Err(SipLoadTestError::MaxDialogsReached(_)) => {
                self.stats.record_failure();
                return Ok(());
            }
            Err(e) => return Err(e),
        };

        let request = self.build_invite_request(&dialog, user);

        if let Some(mut d) = self.dialog_manager.get_dialog_mut(&dialog.call_id) {
            d.state = DialogState::Trying;
            d.original_request = Some(SipMessage::Request(request.clone()));
        }

        self.stats.increment_active_dialogs();

        // TransactionManager経由で送信（再送制御あり）、なければ直接送信
        if let Some(ref tm) = self.transaction_manager {
            tm.send_request(request, self.config.proxy_addr).await?;
        } else {
            let bytes = format_sip_message(&SipMessage::Request(request));
            self.transport.send_to(&bytes, self.config.proxy_addr).await?;
        }

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
        let call_ids = self.dialog_manager.collect_timed_out(Duration::ZERO);
        let mut timed_out = 0;

        for call_id in call_ids {
            self.dialog_manager.remove_dialog(&call_id);
            self.stats.record_failure();
            self.stats.decrement_active_dialogs();
            timed_out += 1;
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

        // TransactionManagerが有効な場合、先にトランザクション層で処理する
        if let Some(ref tm) = self.transaction_manager {
            match tm.handle_response(resp) {
                Some(crate::transaction::TransactionEvent::Response(_tx_id, ref _tx_resp)) => {
                    // TransactionEvent::Responseを受け取った場合のみ既存ロジックに渡す
                }
                Some(crate::transaction::TransactionEvent::Timeout(_tx_id)) => {
                    // タイムアウトはStatsCollectorに記録済み（TransactionManager内で）
                    return Ok(());
                }
                _ => {
                    // 1xx暫定レスポンスやトランザクション不明の場合はスキップ
                    // ただし1xxは既存ロジックでも無視されるので、そのまま通す
                    // トランザクション不明（None）の場合は既存ロジックにフォールスルー
                }
            }
        }

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
        let timeout = self.config.dialog_timeout;
        let call_ids = self.dialog_manager.collect_timed_out(timeout);
        let mut timed_out = 0;

        for call_id in call_ids {
            self.dialog_manager.remove_dialog(&call_id);
            self.stats.record_failure();
            self.stats.decrement_active_dialogs();
            timed_out += 1;
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

    /// Run a batch BYE sending loop that periodically collects expired Confirmed
    /// dialogs and sends BYE requests in batch, replacing individual `tokio::spawn`
    /// per call. The loop ticks every 50ms and stops when `shutdown` flag is set.
    pub async fn start_bye_batch_loop(
        &self,
        call_duration: Duration,
        shutdown: Arc<AtomicBool>,
    ) {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        loop {
            interval.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            let expired = self.dialog_manager.collect_expired_for_bye(call_duration);
            for (call_id, from_tag, to_tag, cseq) in expired {
                let bye = build_bye_request_static(
                    &call_id,
                    &from_tag,
                    to_tag.as_deref(),
                    cseq + 1,
                    self.config.local_addr,
                );
                let bye_bytes = format_sip_message(&SipMessage::Request(bye));
                let _ = self.transport.send_to(&bye_bytes, self.config.proxy_addr).await;

                // Update dialog state to Trying (waiting for BYE response)
                if let Some(mut d) = self.dialog_manager.get_dialog_mut(&call_id) {
                    d.state = DialogState::Trying;
                    d.cseq = cseq + 1;
                }
            }
        }
    }

    /// Run a transaction tick loop that periodically calls `TransactionManager::tick()`
    /// and `cleanup_terminated()` every 10ms. Stops when `shutdown` flag is set.
    /// If no TransactionManager is configured, returns immediately.
    /// When `tick()` returns `TransactionEvent::Timeout`, it is already recorded
    /// in StatsCollector by the TransactionManager internally.
    pub async fn start_transaction_tick_loop(
        &self,
        shutdown: Arc<AtomicBool>,
    ) {
        let tm = match &self.transaction_manager {
            Some(tm) => tm,
            None => return,
        };

        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Process expired timers (retransmissions, timeouts)
            let _events = tm.tick().await;

            // Remove terminated transactions from the map
            tm.cleanup_terminated();
        }
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
                            proxy_addr,
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

        // BYE sending is now handled by the batch BYE loop (start_bye_batch_loop).
        // The batch loop periodically checks for expired Confirmed dialogs and sends
        // BYE requests in batch, replacing the individual tokio::spawn per call.

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
        new_req.headers.set("CSeq", build_cseq_value(new_cseq, method_str));
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
        let branch = crate::sip::generate_branch(&dialog.call_id, dialog.cseq, "REGISTER");
        let mut headers = Headers::new();
        headers.add("Via", build_via_value(self.config.local_addr, &branch));
        headers.set("From", build_from_value(&user.username, &user.domain, &dialog.from_tag));
        headers.set("To", build_to_value(&user.username, &user.domain));
        headers.set("Call-ID", dialog.call_id.clone());
        headers.set("CSeq", build_cseq_value(dialog.cseq, "REGISTER"));
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Content-Length", "0".to_string());

        SipRequest {
            method: Method::Register,
            request_uri: build_sip_domain_uri(&user.domain),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        }
    }

    /// Build an INVITE request
    fn build_invite_request(&self, dialog: &Dialog, user: &UserEntry) -> SipRequest {
        let branch = crate::sip::generate_branch(&dialog.call_id, dialog.cseq, "INVITE");
        let mut headers = Headers::new();
        headers.add("Via", build_via_value(self.config.local_addr, &branch));
        headers.set("From", build_from_value(&user.username, &user.domain, &dialog.from_tag));
        headers.set("To", build_to_value(&user.username, &user.domain));
        headers.set("Call-ID", dialog.call_id.clone());
        headers.set("CSeq", build_cseq_value(dialog.cseq, "INVITE"));
        headers.set("Max-Forwards", "70".to_string());
        headers.set("Session-Expires", self.config.session_expires.as_secs().to_string());
        headers.set("Content-Length", "0".to_string());

        SipRequest {
            method: Method::Invite,
            request_uri: build_sip_uri(&user.username, &user.domain),
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
        let branch = crate::sip::generate_branch(call_id, cseq, "ACK");
        let mut headers = Headers::new();
        headers.add("Via", build_via_value(self.config.local_addr, &branch));
        headers.set("Call-ID", call_id.to_string());
        headers.set("From", build_from_tag_only("sip:uac@local", from_tag));
        headers.set("To", build_to_value_with_optional_tag("sip:uas@remote", to_tag));
        headers.set("CSeq", build_cseq_value(cseq, "ACK"));
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




#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
    use crate::sip::parser::parse_sip_message;
    use crate::stats::StatsCollector;
    use crate::user_pool::{UserPool, UsersFile, UserEntry};
    use crate::dialog::{Dialog, DialogManager, DialogState};
    use std::net::SocketAddr;
    use std::sync::Arc;
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
                session_expires: Duration::from_secs(300),
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
                session_expires: Duration::from_secs(300),
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
                session_expires: Duration::from_secs(300),
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
            let request = build_reinvite_request_static(&call_id, &from_tag, None, 1, local_addr, proxy_addr);

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
                session_expires: Duration::from_secs(300),
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
                session_expires: Duration::from_secs(300),
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
            let proxy_addr: SocketAddr = "127.0.0.1:5060".parse().unwrap();
            let invite = build_reinvite_request_static(
                &call_id, &from_tag, Some("remote-tag"), cseq1, local_addr, proxy_addr,
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

    use crate::testutil::MockTransport;

    /// uac 用の MockTransport ヘルパーメソッド拡張
    trait MockTransportUacExt {
        fn sent_requests(&self) -> Vec<SipRequest>;
    }

    impl MockTransportUacExt for MockTransport {
        fn sent_requests(&self) -> Vec<SipRequest> {
            self.sent_messages()
                .into_iter()
                .filter_map(|(msg, _)| match msg {
                    SipMessage::Request(r) => Some(r),
                    _ => None,
                })
                .collect()
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
        assert_eq!(config.session_expires, Duration::from_secs(300));
    }

    #[test]
    fn test_uac_config_custom() {
        let config = UacConfig {
            proxy_addr: "10.0.0.1:5080".parse().unwrap(),
            local_addr: "10.0.0.2:5070".parse().unwrap(),
            call_duration: Duration::from_secs(10),
            dialog_timeout: Duration::from_secs(60),
            session_expires: Duration::from_secs(600),
        };
        assert_eq!(config.proxy_addr.port(), 5080);
        assert_eq!(config.local_addr.port(), 5070);
        assert_eq!(config.call_duration, Duration::from_secs(10));
        assert_eq!(config.session_expires, Duration::from_secs(600));
    }

    // ===== Uac construction tests =====

    #[test]
    fn test_uac_new_creates_instance() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        // Just verify it constructs without panic
        assert_eq!(uac.config.proxy_addr, test_addr());
    }

    #[test]
    fn test_uac_dialog_manager_accessor() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport);
        // dialog_manager() should return the same Arc instance
        assert!(Arc::ptr_eq(uac.dialog_manager(), &dm));
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

    #[tokio::test]
    async fn test_send_invite_max_dialogs_reached_records_failure() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(1)); // max_dialogs = 1
        let stats = Arc::new(StatsCollector::new());
        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );

        // First invite succeeds
        uac.send_invite().await.unwrap();
        assert_eq!(stats.snapshot().failed_calls, 0);

        // Second invite hits max_dialogs - should record failure, not propagate error
        let result = uac.send_invite().await;
        assert!(result.is_ok());
        assert_eq!(stats.snapshot().failed_calls, 1);
    }

    #[tokio::test]
    async fn test_send_register_max_dialogs_reached_records_failure() {
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(1)); // max_dialogs = 1
        let stats = Arc::new(StatsCollector::new());
        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
        );

        // First register succeeds
        uac.send_register().await.unwrap();
        assert_eq!(stats.snapshot().failed_calls, 0);

        // Second register hits max_dialogs - should record failure, not propagate error
        let result = uac.send_register().await;
        assert!(result.is_ok());
        assert_eq!(stats.snapshot().failed_calls, 1);
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
        let _result = uac.bg_register(3, Duration::from_millis(100)).await;

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

    // ===== Uac method message construction tests =====

    /// End-to-end: optimized build_register_request produces parseable message
    #[test]
    fn test_optimized_register_request_is_parseable() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        let dialog = Dialog {
            call_id: "test-call-id-001".to_string(),
            from_tag: "tag001".to_string(),
            to_tag: None,
            state: DialogState::Trying,
            cseq: 1,
            created_at: Instant::now(),
            session_expires: None,
            auth_attempted: false,
            original_request: None,
        };
        let user = UserEntry {
            username: "alice".to_string(),
            domain: "example.com".to_string(),
            password: "secret".to_string(),
        };

        let request = uac.build_register_request(&dialog, &user);
        let msg = SipMessage::Request(request);
        let bytes = format_sip_message(&msg);
        let parsed = parse_sip_message(&bytes).expect("Optimized REGISTER must be parseable");

        match parsed {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Register);
                assert_eq!(req.headers.get("Call-ID").unwrap(), "test-call-id-001");
                assert_eq!(req.headers.get("CSeq").unwrap(), "1 REGISTER");
            }
            _ => panic!("Expected Request, got Response"),
        }
    }

    /// End-to-end: optimized build_invite_request produces parseable message
    #[test]
    fn test_optimized_invite_request_is_parseable() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        let dialog = Dialog {
            call_id: "test-call-id-002".to_string(),
            from_tag: "tag002".to_string(),
            to_tag: None,
            state: DialogState::Trying,
            cseq: 1,
            created_at: Instant::now(),
            session_expires: None,
            auth_attempted: false,
            original_request: None,
        };
        let user = UserEntry {
            username: "bob".to_string(),
            domain: "sip.example.com".to_string(),
            password: "secret".to_string(),
        };

        let request = uac.build_invite_request(&dialog, &user);
        let msg = SipMessage::Request(request);
        let bytes = format_sip_message(&msg);
        let parsed = parse_sip_message(&bytes).expect("Optimized INVITE must be parseable");

        match parsed {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Invite);
                assert_eq!(req.headers.get("Call-ID").unwrap(), "test-call-id-002");
                assert_eq!(req.headers.get("CSeq").unwrap(), "1 INVITE");
            }
            _ => panic!("Expected Request, got Response"),
        }
    }

    /// build_invite_request adds Session-Expires header with config value (Requirements 3.1, 3.2)
    #[test]
    fn test_build_invite_request_has_session_expires_header() {
        let transport = Arc::new(MockTransport::new());
        let config = UacConfig {
            session_expires: Duration::from_secs(1800),
            ..UacConfig::default()
        };
        let uac = Uac::new(
            transport,
            Arc::new(DialogManager::new(100)),
            Arc::new(StatsCollector::new()),
            make_user_pool(),
            config,
        );
        let dialog = Dialog {
            call_id: "test-session-expires".to_string(),
            from_tag: "tag-se".to_string(),
            to_tag: None,
            state: DialogState::Trying,
            cseq: 1,
            created_at: Instant::now(),
            session_expires: None,
            auth_attempted: false,
            original_request: None,
        };
        let user = UserEntry {
            username: "alice".to_string(),
            domain: "example.com".to_string(),
            password: "pass".to_string(),
        };

        let request = uac.build_invite_request(&dialog, &user);

        let session_expires = request.headers.get("Session-Expires")
            .expect("Session-Expires header must be present");
        assert_eq!(session_expires, "1800");
    }

    /// build_invite_request with default config has Session-Expires 300 (Requirements 3.1, 3.2)
    #[test]
    fn test_build_invite_request_default_session_expires() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        let dialog = Dialog {
            call_id: "test-default-se".to_string(),
            from_tag: "tag-dse".to_string(),
            to_tag: None,
            state: DialogState::Trying,
            cseq: 1,
            created_at: Instant::now(),
            session_expires: None,
            auth_attempted: false,
            original_request: None,
        };
        let user = UserEntry {
            username: "bob".to_string(),
            domain: "sip.example.com".to_string(),
            password: "secret".to_string(),
        };

        let request = uac.build_invite_request(&dialog, &user);

        let session_expires = request.headers.get("Session-Expires")
            .expect("Session-Expires header must be present");
        assert_eq!(session_expires, "300");
    }


    /// End-to-end: optimized build_ack_request produces parseable message
    #[test]
    fn test_optimized_ack_request_is_parseable() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);

        let request = uac.build_ack_request("test-call-id-003", "tag003", Some("remote-tag"), 1);
        let msg = SipMessage::Request(request);
        let bytes = format_sip_message(&msg);
        let parsed = parse_sip_message(&bytes).expect("Optimized ACK must be parseable");

        match parsed {
            SipMessage::Request(req) => {
                assert_eq!(req.method, Method::Ack);
                assert_eq!(req.headers.get("Call-ID").unwrap(), "test-call-id-003");
                assert_eq!(req.headers.get("CSeq").unwrap(), "1 ACK");
            }
            _ => panic!("Expected Request, got Response"),
        }
    }

    // ===== Batch BYE loop tests =====

    #[tokio::test]
    async fn test_bye_batch_loop_sends_bye_for_expired_confirmed_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());
        let uac = Arc::new(uac);

        // Create a dialog and set it to Confirmed with a to_tag
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        if let Some(mut dialog) = dm.get_dialog_mut(&call_id) {
            dialog.to_tag = Some("remote-tag".to_string());
        }

        // Use a very short call_duration so the dialog is immediately expired
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let uac_clone = uac.clone();
        let handle = tokio::spawn(async move {
            uac_clone.start_bye_batch_loop(
                Duration::from_millis(0),
                shutdown_clone,
            ).await;
        });

        // Wait enough time for at least one batch tick
        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.await;

        // Verify that a BYE was sent
        let requests = transport.sent_requests();
        let bye_requests: Vec<_> = requests.iter().filter(|r| r.method == Method::Bye).collect();
        assert!(!bye_requests.is_empty(), "At least one BYE should have been sent");
        assert_eq!(bye_requests[0].headers.get("Call-ID").unwrap(), call_id);
    }

    #[tokio::test]
    async fn test_bye_batch_loop_does_not_send_bye_for_non_expired_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());
        let uac = Arc::new(uac);

        // Create a Confirmed dialog
        let d = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d.call_id, DialogState::Confirmed).unwrap();
        if let Some(mut dialog) = dm.get_dialog_mut(&d.call_id) {
            dialog.to_tag = Some("tag".to_string());
        }

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Use a very long call_duration so the dialog is NOT expired
        let uac_clone = uac.clone();
        let handle = tokio::spawn(async move {
            uac_clone.start_bye_batch_loop(
                Duration::from_secs(3600),
                shutdown_clone,
            ).await;
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.await;

        let requests = transport.sent_requests();
        let bye_requests: Vec<_> = requests.iter().filter(|r| r.method == Method::Bye).collect();
        assert!(bye_requests.is_empty(), "No BYE should be sent for non-expired dialogs");
    }

    #[tokio::test]
    async fn test_bye_batch_loop_stops_on_shutdown_flag() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_parts(transport.clone());
        let uac = Arc::new(uac);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let uac_clone = uac.clone();
        let handle = tokio::spawn(async move {
            uac_clone.start_bye_batch_loop(
                Duration::from_secs(3),
                shutdown_clone,
            ).await;
        });

        // Set shutdown immediately
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        // The loop should exit quickly
        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "Batch loop should stop promptly when shutdown flag is set");
    }

    #[tokio::test]
    async fn test_bye_batch_loop_updates_dialog_state_after_bye() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_parts(transport.clone());
        let uac = Arc::new(uac);

        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        if let Some(mut dialog) = dm.get_dialog_mut(&call_id) {
            dialog.to_tag = Some("tag".to_string());
        }

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let uac_clone = uac.clone();
        let handle = tokio::spawn(async move {
            uac_clone.start_bye_batch_loop(
                Duration::from_millis(0),
                shutdown_clone,
            ).await;
        });

        tokio::time::sleep(Duration::from_millis(120)).await;
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = handle.await;

        // After BYE is sent, dialog state should be updated to Trying
        let state = dm.get_dialog(&call_id).map(|d| d.state.clone());
        if let Some(s) = state {
            assert_eq!(s, DialogState::Trying,
                "Dialog state should be Trying after BYE is sent");
        }
    }

    #[tokio::test]
    async fn test_handle_invite_success_no_longer_spawns_bye_task() {
        // After optimization, handle_invite_success should NOT spawn individual BYE tasks.
        // Instead, the batch loop handles BYE sending.
        // We verify by checking that no BYE is sent within the call_duration window
        // (only ACK should be sent immediately).
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let config = UacConfig {
            call_duration: Duration::from_secs(3600), // very long, BYE should not be sent
            ..UacConfig::default()
        };
        let uac = Uac::new(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            config,
        );

        // Create a dialog in Trying state (simulating INVITE sent)
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Trying).unwrap();

        // Build a 200 OK response
        let resp = make_response(200, "OK", &call_id, "1 INVITE");
        uac.handle_response(resp, test_addr()).await.unwrap();

        // Wait a bit to ensure no BYE task was spawned
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Only ACK should have been sent, no BYE
        let requests = transport.sent_requests();
        let ack_count = requests.iter().filter(|r| r.method == Method::Ack).count();
        let bye_count = requests.iter().filter(|r| r.method == Method::Bye).count();
        assert_eq!(ack_count, 1, "ACK should still be sent immediately");
        assert_eq!(bye_count, 0, "BYE should NOT be spawned individually anymore");
    }

    // ===== TransactionManager統合テスト =====
    // **Validates: Requirements 8.1, 8.2, 8.3, 8.4, 8.5**

    fn make_uac_with_transaction_manager(
        transport: Arc<MockTransport>,
    ) -> (Uac, Arc<DialogManager>, Arc<StatsCollector>) {
        use crate::transaction::{TimerConfig, TransactionManager};
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let tm = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            TimerConfig::default(),
            10000,
        );
        let uac = Uac::with_transaction_manager(
            transport,
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
            tm,
        );
        (uac, dm, stats)
    }

    #[tokio::test]
    async fn test_send_register_via_transaction_manager() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_transaction_manager(transport.clone());

        uac.send_register().await.unwrap();

        // TransactionManager経由でもtransportに送信される
        assert_eq!(transport.sent_count(), 1);
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Register);
    }

    #[tokio::test]
    async fn test_send_invite_via_transaction_manager() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_transaction_manager(transport.clone());

        uac.send_invite().await.unwrap();

        assert_eq!(transport.sent_count(), 1);
        let requests = transport.sent_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::Invite);
    }

    #[tokio::test]
    async fn test_send_register_via_tm_creates_transaction() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_transaction_manager(transport.clone());

        uac.send_register().await.unwrap();

        // TransactionManagerにトランザクションが登録されている
        let tm = uac.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1);
    }

    #[tokio::test]
    async fn test_send_invite_via_tm_creates_transaction() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_transaction_manager(transport.clone());

        uac.send_invite().await.unwrap();

        let tm = uac.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1);
    }

    #[tokio::test]
    async fn test_handle_response_via_tm_register_200() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, stats) = make_uac_with_transaction_manager(transport.clone());

        // REGISTERを送信してダイアログとトランザクションを作成
        uac.send_register().await.unwrap();

        // 送信されたリクエストからVia branchとCall-IDを取得
        let requests = transport.sent_requests();
        let req = &requests[0];
        let call_id = req.headers.get("Call-ID").unwrap().to_string();
        let via = req.headers.get("Via").unwrap().to_string();
        let cseq = req.headers.get("CSeq").unwrap().to_string();

        // 正しいbranchを持つ200 OKレスポンスを作成
        let mut headers = Headers::new();
        headers.add("Via", via);
        headers.set("Call-ID", call_id.clone());
        headers.set("CSeq", cseq);
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:alice@example.com>;tag=def".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        uac.handle_response(response, test_addr()).await.unwrap();

        // REGISTER 200 OK → ダイアログが削除され、成功が記録される
        assert!(dm.get_dialog(&call_id).is_none());
        assert_eq!(stats.snapshot().successful_calls, 1);
    }

    #[tokio::test]
    async fn test_handle_response_via_tm_invite_200_sends_ack() {
        let transport = Arc::new(MockTransport::new());
        let (uac, dm, _stats) = make_uac_with_transaction_manager(transport.clone());

        uac.send_invite().await.unwrap();

        let requests = transport.sent_requests();
        let req = &requests[0];
        let call_id = req.headers.get("Call-ID").unwrap().to_string();
        let via = req.headers.get("Via").unwrap().to_string();
        let cseq = req.headers.get("CSeq").unwrap().to_string();

        let mut headers = Headers::new();
        headers.add("Via", via);
        headers.set("Call-ID", call_id.clone());
        headers.set("CSeq", cseq);
        headers.set("From", "<sip:alice@example.com>;tag=abc".to_string());
        headers.set("To", "<sip:alice@example.com>;tag=def".to_string());
        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        });

        uac.handle_response(response, test_addr()).await.unwrap();

        // INVITE 200 OK → ACKが送信される
        let all_requests = transport.sent_requests();
        let ack_count = all_requests.iter().filter(|r| r.method == Method::Ack).count();
        assert_eq!(ack_count, 1, "ACK should be sent for INVITE 200 OK");

        // ダイアログがConfirmed状態になる
        let dialog = dm.get_dialog(&call_id);
        assert!(dialog.is_some());
        assert_eq!(dialog.unwrap().state, DialogState::Confirmed);
    }

    #[tokio::test]
    async fn test_with_transaction_manager_constructor() {
        use crate::transaction::{TimerConfig, TransactionManager};
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let tm = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            TimerConfig::default(),
            10000,
        );
        let uac = Uac::with_transaction_manager(
            transport,
            dm,
            stats,
            make_user_pool(),
            UacConfig::default(),
            tm,
        );
        assert!(uac.transaction_manager.is_some());
    }

    #[test]
    fn test_uac_new_has_no_transaction_manager() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        assert!(uac.transaction_manager.is_none());
    }

    // ===== Task 14.2: Transaction tick loop tests =====

    #[tokio::test]
    async fn test_transaction_tick_loop_stops_on_shutdown() {
        let transport = Arc::new(MockTransport::new());
        let (uac, _dm, _stats) = make_uac_with_transaction_manager(transport.clone());
        let uac = Arc::new(uac);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn({
            let uac = uac.clone();
            async move {
                uac.start_transaction_tick_loop(shutdown_clone).await;
            }
        });

        // Let the loop run briefly, then signal shutdown
        tokio::time::sleep(Duration::from_millis(30)).await;
        shutdown.store(true, Ordering::Relaxed);

        // The loop should stop within a reasonable time
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "tick loop should stop after shutdown signal");
    }

    #[tokio::test]
    async fn test_transaction_tick_loop_without_tm_returns_immediately() {
        let transport = Arc::new(MockTransport::new());
        let uac = make_uac(transport);
        let uac = Arc::new(uac);

        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn({
            let uac = uac.clone();
            let shutdown = shutdown.clone();
            async move {
                uac.start_transaction_tick_loop(shutdown).await;
            }
        });

        // Without TransactionManager, the loop should return immediately
        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "tick loop should return immediately without TM");
    }

    #[tokio::test]
    async fn test_transaction_tick_loop_calls_cleanup_terminated() {
        use crate::transaction::{TimerConfig, TransactionManager};
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());
        let tm = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            TimerConfig::default(),
            10000,
        );

        let uac = Uac::with_transaction_manager(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
            tm,
        );
        let uac = Arc::new(uac);

        // Send a request to create a transaction
        uac.send_register().await.unwrap();
        let tm = uac.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn({
            let uac = uac.clone();
            async move {
                uac.start_transaction_tick_loop(shutdown_clone).await;
            }
        });

        // Let the tick loop run for a bit (transactions won't terminate without
        // timer expiry, but the loop should be running and calling tick/cleanup)
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.store(true, Ordering::Relaxed);

        let result = tokio::time::timeout(Duration::from_secs(1), handle).await;
        assert!(result.is_ok(), "tick loop should stop after shutdown signal");
    }

    #[tokio::test]
    async fn test_transaction_tick_loop_records_timeout_stats() {
        use crate::transaction::{TimerConfig, TransactionManager};
        let transport = Arc::new(MockTransport::new());
        let dm = Arc::new(DialogManager::new(100));
        let stats = Arc::new(StatsCollector::new());

        // Use very short T1 so Timer_F (64*T1 = 64ms) fires quickly
        let timer_config = TimerConfig {
            t1: Duration::from_millis(1),
            t2: Duration::from_millis(4),
            t4: Duration::from_millis(5),
        };
        let tm = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            timer_config,
            10000,
        );

        let uac = Uac::with_transaction_manager(
            transport.clone(),
            dm.clone(),
            stats.clone(),
            make_user_pool(),
            UacConfig::default(),
            tm,
        );
        let uac = Arc::new(uac);

        // Send a REGISTER (Non-INVITE → Timer_F = 64*1ms = 64ms)
        uac.send_register().await.unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = tokio::spawn({
            let uac = uac.clone();
            async move {
                uac.start_transaction_tick_loop(shutdown_clone).await;
            }
        });

        // Wait for Timer_F to expire (64ms + some margin)
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.store(true, Ordering::Relaxed);

        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;

        // Timer_F timeout should have been recorded
        let snap = stats.snapshot();
        assert!(snap.transaction_timeouts >= 1, "timeout should be recorded by tick loop, got {}", snap.transaction_timeouts);
    }

    // ===== Property 10: 構築メッセージのパース可能性テスト =====
    // **Validates: Requirements 8.1**
    //
    // For any valid dialog parameters, SIP messages constructed by the UAC's
    // build functions must be parseable by parse_sip_message and contain the
    // correct method, Call-ID, and CSeq value.

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        /// Feature: performance-bottleneck-optimization, Property 10: 構築メッセージのパース可能性
        /// 任意のdialogパラメータで構築したSIPメッセージがparse_sip_messageでパース可能かつ
        /// 正しいメソッド・Call-ID・CSeqを含む
        #[test]
        fn pbt_built_message_parseable(
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
            cseq in arb_cseq(),
            local_addr in arb_socket_addr(),
            username in arb_username(),
            domain in arb_domain(),
        ) {
            use crate::sip::formatter::format_sip_message;

            // --- REGISTER ---
            {
                let branch = crate::sip::generate_branch(&call_id, cseq, "REGISTER");
                let mut headers = Headers::new();
                headers.add("Via", build_via_value(local_addr, &branch));
                headers.set("From", build_from_value(&username, &domain, &from_tag));
                headers.set("To", build_to_value(&username, &domain));
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", build_cseq_value(cseq, "REGISTER"));
                headers.set("Max-Forwards", "70".to_string());
                headers.set("Content-Length", "0".to_string());

                let request = SipRequest {
                    method: Method::Register,
                    request_uri: build_sip_domain_uri(&domain),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                };
                let bytes = format_sip_message(&SipMessage::Request(request));
                let parsed = parse_sip_message(&bytes);
                prop_assert!(parsed.is_ok(), "REGISTER must be parseable, got: {:?}", parsed.err());
                match parsed.unwrap() {
                    SipMessage::Request(req) => {
                        prop_assert_eq!(req.method, Method::Register, "Method must be REGISTER");
                        prop_assert_eq!(req.headers.get("Call-ID").unwrap(), call_id.clone(), "Call-ID must match");
                        let cseq_val = req.headers.get("CSeq").unwrap();
                        let cseq_num: u32 = cseq_val.split_whitespace().next().unwrap().parse().unwrap();
                        prop_assert_eq!(cseq_num, cseq, "CSeq number must match");
                    }
                    _ => prop_assert!(false, "Expected Request, got Response"),
                }
            }

            // --- INVITE ---
            {
                let branch = crate::sip::generate_branch(&call_id, cseq, "INVITE");
                let mut headers = Headers::new();
                headers.add("Via", build_via_value(local_addr, &branch));
                headers.set("From", build_from_value(&username, &domain, &from_tag));
                headers.set("To", build_to_value(&username, &domain));
                headers.set("Call-ID", call_id.clone());
                headers.set("CSeq", build_cseq_value(cseq, "INVITE"));
                headers.set("Max-Forwards", "70".to_string());
                headers.set("Content-Length", "0".to_string());

                let request = SipRequest {
                    method: Method::Invite,
                    request_uri: build_sip_uri(&username, &domain),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                };
                let bytes = format_sip_message(&SipMessage::Request(request));
                let parsed = parse_sip_message(&bytes);
                prop_assert!(parsed.is_ok(), "INVITE must be parseable, got: {:?}", parsed.err());
                match parsed.unwrap() {
                    SipMessage::Request(req) => {
                        prop_assert_eq!(req.method, Method::Invite, "Method must be INVITE");
                        prop_assert_eq!(req.headers.get("Call-ID").unwrap(), call_id.clone(), "Call-ID must match");
                        let cseq_val = req.headers.get("CSeq").unwrap();
                        let cseq_num: u32 = cseq_val.split_whitespace().next().unwrap().parse().unwrap();
                        prop_assert_eq!(cseq_num, cseq, "CSeq number must match");
                    }
                    _ => prop_assert!(false, "Expected Request, got Response"),
                }
            }

            // --- ACK ---
            {
                let branch = crate::sip::generate_branch(&call_id, cseq, "ACK");
                let mut headers = Headers::new();
                headers.add("Via", build_via_value(local_addr, &branch));
                headers.set("Call-ID", call_id.clone());
                headers.set("From", build_from_tag_only("sip:uac@local", &from_tag));
                headers.set("To", build_to_value_with_optional_tag("sip:uas@remote", None));
                headers.set("CSeq", build_cseq_value(cseq, "ACK"));
                headers.set("Max-Forwards", "70".to_string());
                headers.set("Content-Length", "0".to_string());

                let request = SipRequest {
                    method: Method::Ack,
                    request_uri: "sip:uas@remote".to_string(),
                    version: "SIP/2.0".to_string(),
                    headers,
                    body: None,
                };
                let bytes = format_sip_message(&SipMessage::Request(request));
                let parsed = parse_sip_message(&bytes);
                prop_assert!(parsed.is_ok(), "ACK must be parseable, got: {:?}", parsed.err());
                match parsed.unwrap() {
                    SipMessage::Request(req) => {
                        prop_assert_eq!(req.method, Method::Ack, "Method must be ACK");
                        prop_assert_eq!(req.headers.get("Call-ID").unwrap(), call_id.clone(), "Call-ID must match");
                        let cseq_val = req.headers.get("CSeq").unwrap();
                        let cseq_num: u32 = cseq_val.split_whitespace().next().unwrap().parse().unwrap();
                        prop_assert_eq!(cseq_num, cseq, "CSeq number must match");
                    }
                    _ => prop_assert!(false, "Expected Request, got Response"),
                }
            }

            // --- BYE ---
            {
                let request = build_bye_request_static(&call_id, &from_tag, None, cseq, local_addr);
                let bytes = format_sip_message(&SipMessage::Request(request));
                let parsed = parse_sip_message(&bytes);
                prop_assert!(parsed.is_ok(), "BYE must be parseable, got: {:?}", parsed.err());
                match parsed.unwrap() {
                    SipMessage::Request(req) => {
                        prop_assert_eq!(req.method, Method::Bye, "Method must be BYE");
                        prop_assert_eq!(req.headers.get("Call-ID").unwrap(), call_id.clone(), "Call-ID must match");
                        let cseq_val = req.headers.get("CSeq").unwrap();
                        let cseq_num: u32 = cseq_val.split_whitespace().next().unwrap().parse().unwrap();
                        prop_assert_eq!(cseq_num, cseq, "CSeq number must match");
                    }
                    _ => prop_assert!(false, "Expected Request, got Response"),
                }
            }
        }
    }

    // ===== Feature: bench-auth-session-timer, Property 2: build_invite_requestのSession-Expiresヘッダ付与 =====
    // **Validates: Requirements 3.1, 3.2**
    //
    // For all valid UacConfig (with arbitrary session_expires), Dialog, and UserEntry,
    // build_invite_request must include a Session-Expires header whose value equals
    // UacConfig.session_expires.as_secs() as a string.

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn pbt_build_invite_request_session_expires_header(
            (proxy_addr, local_addr) in arb_distinct_addr_pair(),
            session_expires_secs in 1u64..=86400,
            call_id in arb_call_id(),
            from_tag in arb_from_tag(),
            cseq in arb_cseq(),
            username in arb_username(),
            domain in arb_domain(),
        ) {
            let config = UacConfig {
                proxy_addr,
                local_addr,
                call_duration: Duration::from_secs(3),
                dialog_timeout: Duration::from_secs(32),
                session_expires: Duration::from_secs(session_expires_secs),
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
                call_id,
                from_tag,
                to_tag: None,
                state: DialogState::Trying,
                cseq,
                created_at: Instant::now(),
                session_expires: None,
                auth_attempted: false,
                original_request: None,
            };
            let user = UserEntry {
                username,
                domain,
                password: "test_pass".to_string(),
            };

            let request = uac.build_invite_request(&dialog, &user);

            // Assert: Session-Expires header must be present
            let se_header = request.headers.get("Session-Expires");
            prop_assert!(se_header.is_some(), "Session-Expires header must be present in INVITE request");

            // Assert: Session-Expires value must match config
            let se_value = se_header.unwrap();
            let expected = session_expires_secs.to_string();
            prop_assert_eq!(
                se_value,
                &expected,
                "Session-Expires header value ({}) must equal UacConfig.session_expires.as_secs() ({})",
                se_value,
                expected
            );
        }
    }
}
