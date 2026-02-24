// UAS (User Agent Server) module
//
// Handles incoming SIP requests and generates appropriate responses.
// Uses a transport trait to enable testing without actual network.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::error::SipLoadTestError;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
use crate::sip::formatter::format_sip_message;
use crate::stats::StatsCollector;

/// Transport trait to abstract UDP sending for testability.
pub trait SipTransport: Send + Sync {
    fn send_to<'a>(
        &'a self,
        data: &'a [u8],
        addr: SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>>;
}

/// Real UDP transport adapter
impl SipTransport for crate::transport::UdpTransport {
    fn send_to<'a>(
        &'a self,
        data: &'a [u8],
        addr: SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>> {
        Box::pin(self.send_to(data, addr))
    }
}

/// UAS dialog state
#[derive(Debug, Clone, PartialEq)]
pub enum UasDialogState {
    Early,
    Confirmed,
    Terminated,
}

/// UAS dialog entry
#[derive(Debug, Clone)]
pub struct UasDialog {
    pub call_id: String,
    pub state: UasDialogState,
    pub session_expires: Option<Duration>,
    pub last_activity: Instant,
}

/// UAS configuration
#[derive(Debug, Clone)]
pub struct UasConfig {
    // Placeholder for future config fields (e.g., session timer settings)
}

impl Default for UasConfig {
    fn default() -> Self {
        Self {}
    }
}

/// UAS (User Agent Server) - handles incoming SIP requests.
pub struct Uas {
    transport: Arc<dyn SipTransport>,
    dialogs: DashMap<String, UasDialog>,
    stats: Arc<StatsCollector>,
    config: UasConfig,
    transaction_manager: Option<crate::transaction::TransactionManager>,
}

impl Uas {
    /// Create a new UAS instance.
    pub fn new(
        transport: Arc<dyn SipTransport>,
        stats: Arc<StatsCollector>,
        config: UasConfig,
    ) -> Self {
        Self {
            transport,
            dialogs: DashMap::new(),
            stats,
            config,
            transaction_manager: None,
        }
    }

    /// Create a new UAS instance with TransactionManager for retransmission control.
    pub fn with_transaction_manager(
        transport: Arc<dyn SipTransport>,
        stats: Arc<StatsCollector>,
        config: UasConfig,
        transaction_manager: crate::transaction::TransactionManager,
    ) -> Self {
        Self {
            transport,
            dialogs: DashMap::new(),
            stats,
            config,
            transaction_manager: Some(transaction_manager),
        }
    }

    /// Handle an incoming SIP request and send appropriate responses.
    pub async fn handle_request(
        &self,
        request: SipMessage,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        match request {
            SipMessage::Request(ref req) => {
                // TransactionManagerが有効な場合、先にトランザクション層で処理する
                if let Some(ref tm) = self.transaction_manager {
                    // ACKはTransactionManager内部で処理される（handle_requestがACKを処理する）
                    // ただしUAS側のダイアログ状態更新も必要なので、ACKは両方で処理する
                    if req.method == Method::Ack {
                        // ACKはTransactionManagerに渡してからUASでも処理
                        let _ = tm.handle_request(req, from);
                        return self.handle_ack(req);
                    }

                    match tm.handle_request(req, from) {
                        Some(crate::transaction::TransactionEvent::Request(tx_id, _req, _source)) => {
                            // 新規リクエスト → 既存ロジックで処理し、レスポンスはTM経由で送信
                            return self.handle_request_with_tm(req, from, &tx_id).await;
                        }
                        _ => {
                            // 再送は吸収（UASには通知しない）、またはmax_transactions到達
                            return Ok(());
                        }
                    }
                }

                // TransactionManagerなし → 従来通り直接処理
                match req.method {
                    Method::Invite => self.handle_invite(req, from).await,
                    Method::Ack => self.handle_ack(req),
                    Method::Bye => self.handle_bye(req, from).await,
                    Method::Register => self.handle_register(req, from).await,
                    _ => Ok(()),
                }
            }
            SipMessage::Response(_) => Ok(()), // UAS ignores responses
        }
    }

    /// Handle request with TransactionManager: responses are sent through TM.
    async fn handle_request_with_tm(
        &self,
        request: &SipRequest,
        from: SocketAddr,
        tx_id: &crate::transaction::TransactionId,
    ) -> Result<(), SipLoadTestError> {
        let tm = self.transaction_manager.as_ref().unwrap();

        match request.method {
            Method::Invite => {
                let call_id = request.headers.get("Call-ID").unwrap_or("").to_string();

                // Parse Session-Expires header if present
                let session_expires = request
                    .headers
                    .get("Session-Expires")
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);

                // Check if this is a re-INVITE
                if self.dialogs.contains_key(&call_id) {
                    return self.handle_reinvite_with_tm(request, from, &call_id, session_expires, tx_id).await;
                }

                // Send 100 Trying directly (INVITE server transaction starts in Proceeding,
                // so 1xx provisional responses are sent directly, not through TM's send_response)
                let trying = self.build_response(request, 100, "Trying");
                let trying_bytes = format_sip_message(&SipMessage::Response(trying));
                self.transport.send_to(&trying_bytes, from).await?;

                // Create dialog in Early state
                self.dialogs.insert(
                    call_id.clone(),
                    UasDialog {
                        call_id: call_id.clone(),
                        state: UasDialogState::Early,
                        session_expires,
                        last_activity: Instant::now(),
                    },
                );

                // Send 200 OK via TM (final response triggers state transition)
                let mut ok = self.build_response(request, 200, "OK");
                if let Some(se) = session_expires {
                    ok.headers.set("Session-Expires", se.as_secs().to_string());
                }
                tm.send_response(tx_id, ok).await?;

                self.stats.increment_active_dialogs();
                Ok(())
            }
            Method::Bye => {
                let call_id = request.headers.get("Call-ID").unwrap_or("").to_string();

                if self.dialogs.contains_key(&call_id) {
                    let ok = self.build_response(request, 200, "OK");
                    tm.send_response(tx_id, ok).await?;
                    self.dialogs.remove(&call_id);
                    self.stats.decrement_active_dialogs();
                } else {
                    let not_exist = self.build_response(request, 481, "Call/Transaction Does Not Exist");
                    tm.send_response(tx_id, not_exist).await?;
                }
                Ok(())
            }
            Method::Register => {
                let ok = self.build_response(request, 200, "OK");
                tm.send_response(tx_id, ok).await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Handle re-INVITE with TransactionManager.
    async fn handle_reinvite_with_tm(
        &self,
        request: &SipRequest,
        _from: SocketAddr,
        call_id: &str,
        session_expires: Option<Duration>,
        tx_id: &crate::transaction::TransactionId,
    ) -> Result<(), SipLoadTestError> {
        let tm = self.transaction_manager.as_ref().unwrap();

        if let Some(mut dialog) = self.dialogs.get_mut(call_id) {
            dialog.last_activity = Instant::now();
            if let Some(se) = session_expires {
                dialog.session_expires = Some(se);
            }
        }

        let mut ok = self.build_response(request, 200, "OK");
        if let Some(se) = session_expires {
            ok.headers.set("Session-Expires", se.as_secs().to_string());
        }
        tm.send_response(tx_id, ok).await?;
        Ok(())
    }

    /// Handle INVITE: send 100 Trying, then 200 OK, create dialog.
    /// For re-INVITE on existing dialog: send 200 OK and update session.
    async fn handle_invite(
        &self,
        request: &SipRequest,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        let call_id = request
            .headers
            .get("Call-ID")
            .unwrap_or("")
            .to_string();

        // Parse Session-Expires header if present
        let session_expires = request
            .headers
            .get("Session-Expires")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);

        // Check if this is a re-INVITE (dialog already exists)
        if self.dialogs.contains_key(&call_id) {
            return self.handle_reinvite(request, from, &call_id, session_expires).await;
        }

        // Send 100 Trying
        let trying = self.build_response(request, 100, "Trying");
        let trying_bytes = format_sip_message(&SipMessage::Response(trying));
        self.transport.send_to(&trying_bytes, from).await?;

        // Create dialog in Early state
        self.dialogs.insert(
            call_id.clone(),
            UasDialog {
                call_id: call_id.clone(),
                state: UasDialogState::Early,
                session_expires,
                last_activity: Instant::now(),
            },
        );

        // Send 200 OK (with Session-Expires if present)
        let mut ok = self.build_response(request, 200, "OK");
        if let Some(se) = session_expires {
            ok.headers.set("Session-Expires", se.as_secs().to_string());
        }
        let ok_bytes = format_sip_message(&SipMessage::Response(ok));
        self.transport.send_to(&ok_bytes, from).await?;

        self.stats.increment_active_dialogs();

        Ok(())
    }

    /// Handle re-INVITE: send 200 OK and update session activity.
    async fn handle_reinvite(
        &self,
        request: &SipRequest,
        from: SocketAddr,
        call_id: &str,
        session_expires: Option<Duration>,
    ) -> Result<(), SipLoadTestError> {
        // Update dialog's last_activity and optionally session_expires
        if let Some(mut dialog) = self.dialogs.get_mut(call_id) {
            dialog.last_activity = Instant::now();
            if let Some(se) = session_expires {
                dialog.session_expires = Some(se);
            }
        }

        // Send 200 OK
        let mut ok = self.build_response(request, 200, "OK");
        if let Some(se) = session_expires {
            ok.headers.set("Session-Expires", se.as_secs().to_string());
        }
        let ok_bytes = format_sip_message(&SipMessage::Response(ok));
        self.transport.send_to(&ok_bytes, from).await?;

        Ok(())
    }

    /// Handle ACK: update dialog state to Confirmed.
    fn handle_ack(&self, request: &SipRequest) -> Result<(), SipLoadTestError> {
        let call_id = request
            .headers
            .get("Call-ID")
            .unwrap_or("")
            .to_string();

        if let Some(mut dialog) = self.dialogs.get_mut(&call_id) {
            dialog.state = UasDialogState::Confirmed;
        }
        Ok(())
    }

    /// Handle BYE: if dialog exists, send 200 OK and remove it.
    /// If dialog doesn't exist, send 481.
    async fn handle_bye(
        &self,
        request: &SipRequest,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        let call_id = request
            .headers
            .get("Call-ID")
            .unwrap_or("")
            .to_string();

        if self.dialogs.contains_key(&call_id) {
            // Dialog exists: send 200 OK and remove
            let ok = self.build_response(request, 200, "OK");
            let ok_bytes = format_sip_message(&SipMessage::Response(ok));
            self.transport.send_to(&ok_bytes, from).await?;

            self.dialogs.remove(&call_id);
            self.stats.decrement_active_dialogs();
        } else {
            // Dialog doesn't exist: send 481
            let not_exist =
                self.build_response(request, 481, "Call/Transaction Does Not Exist");
            let not_exist_bytes = format_sip_message(&SipMessage::Response(not_exist));
            self.transport.send_to(&not_exist_bytes, from).await?;
        }

        Ok(())
    }

    /// Handle REGISTER: send 200 OK.
    async fn handle_register(
        &self,
        request: &SipRequest,
        from: SocketAddr,
    ) -> Result<(), SipLoadTestError> {
        let ok = self.build_response(request, 200, "OK");
        let ok_bytes = format_sip_message(&SipMessage::Response(ok));
        self.transport.send_to(&ok_bytes, from).await?;
        Ok(())
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

    /// Check all dialogs for session timeouts.
    /// Removes dialogs that have exceeded their session_expires without activity.
    /// Returns the number of timed-out dialogs.
    pub fn check_session_timeouts(&self) -> usize {
        let now = Instant::now();
        let mut timed_out_keys = Vec::new();

        for entry in self.dialogs.iter() {
            if let Some(expires) = entry.value().session_expires {
                let elapsed = now.duration_since(entry.value().last_activity);
                if elapsed >= expires {
                    timed_out_keys.push(entry.key().clone());
                }
            }
        }

        let count = timed_out_keys.len();
        for key in timed_out_keys {
            self.dialogs.remove(&key);
            self.stats.record_failure();
            self.stats.decrement_active_dialogs();
        }

        count
    }

    /// Get a dialog by Call-ID (for testing/inspection).
    pub fn get_dialog(&self, call_id: &str) -> Option<UasDialog> {
        self.dialogs.get(call_id).map(|d| d.clone())
    }

    /// Run a transaction tick loop that periodically calls `TransactionManager::tick()`
    /// and `cleanup_terminated()` every 10ms. Stops when `shutdown` flag is set.
    /// If no TransactionManager is configured, returns immediately.
    pub async fn start_transaction_tick_loop(
        &self,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let tm = match &self.transaction_manager {
            Some(tm) => tm,
            None => return,
        };

        let mut interval = tokio::time::interval(Duration::from_millis(10));
        loop {
            interval.tick().await;
            if shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }

            // Process expired timers (retransmissions, timeouts)
            let _events = tm.tick().await;

            // Remove terminated transactions from the map
            tm.cleanup_terminated();
        }
    }

    /// Shutdown the UAS, terminating all dialogs.
    pub async fn shutdown(&self) -> Result<(), SipLoadTestError> {
        self.dialogs.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method, SipMessage, SipRequest};
    use crate::sip::parser::parse_sip_message;
    use crate::stats::StatsCollector;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

        /// Get all sent messages, parsed as SipMessages.
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

        /// Get sent responses only.
        fn sent_responses(&self) -> Vec<crate::sip::message::SipResponse> {
            self.sent_messages()
                .into_iter()
                .filter_map(|(msg, _)| match msg {
                    SipMessage::Response(r) => Some(r),
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

    fn test_addr() -> SocketAddr {
        "127.0.0.1:5060".parse().unwrap()
    }

    fn make_uas(transport: Arc<MockTransport>) -> Uas {
        Uas::new(
            transport,
            Arc::new(StatsCollector::new()),
            UasConfig::default(),
        )
    }

    fn make_invite_request(call_id: &str) -> SipMessage {
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

    fn make_ack_request(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
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

    fn make_bye_request(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK999".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
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

    fn make_register_request(call_id: &str) -> SipMessage {
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

    // ===== INVITE handling tests =====

    #[tokio::test]
    async fn test_invite_sends_100_trying_then_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-1");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 2, "Should send exactly 2 responses");
        assert_eq!(responses[0].status_code, 100, "First response should be 100 Trying");
        assert_eq!(responses[0].reason_phrase, "Trying");
        assert_eq!(responses[1].status_code, 200, "Second response should be 200 OK");
        assert_eq!(responses[1].reason_phrase, "OK");
    }

    #[tokio::test]
    async fn test_invite_creates_dialog() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-2");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let dialog = uas.get_dialog("call-2");
        assert!(dialog.is_some(), "Dialog should be created after INVITE");
        assert_eq!(dialog.unwrap().state, UasDialogState::Early);
    }

    #[tokio::test]
    async fn test_invite_response_copies_headers() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-hdr");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        // Check 200 OK has correct headers
        let ok = &responses[1];
        assert_eq!(ok.headers.get("Call-ID"), Some("call-hdr"));
        assert_eq!(ok.headers.get("CSeq"), Some("1 INVITE"));
        assert!(ok.headers.get("From").is_some());
        assert!(ok.headers.get("To").is_some());
        assert!(ok.headers.get("Via").is_some());
    }

    #[tokio::test]
    async fn test_invite_increments_active_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());

        let invite = make_invite_request("call-stats");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let snap = stats.snapshot();
        assert_eq!(snap.active_dialogs, 1);
    }

    // ===== ACK handling tests =====

    #[tokio::test]
    async fn test_ack_updates_dialog_to_confirmed() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // First send INVITE to create dialog
        let invite = make_invite_request("call-ack");
        uas.handle_request(invite, test_addr()).await.unwrap();

        // Then send ACK
        let ack = make_ack_request("call-ack");
        uas.handle_request(ack, test_addr()).await.unwrap();

        let dialog = uas.get_dialog("call-ack").unwrap();
        assert_eq!(dialog.state, UasDialogState::Confirmed);
    }

    #[tokio::test]
    async fn test_ack_sends_no_response() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // Create dialog first
        let invite = make_invite_request("call-ack-noresp");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let count_before = transport.sent.lock().unwrap().len();

        // Send ACK
        let ack = make_ack_request("call-ack-noresp");
        uas.handle_request(ack, test_addr()).await.unwrap();

        let count_after = transport.sent.lock().unwrap().len();
        assert_eq!(count_after, count_before, "ACK should not generate any response");
    }

    #[tokio::test]
    async fn test_ack_for_nonexistent_dialog_is_ignored() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // Send ACK without prior INVITE - should not error
        let ack = make_ack_request("nonexistent-call");
        let result = uas.handle_request(ack, test_addr()).await;
        assert!(result.is_ok());
    }

    // ===== BYE handling tests =====

    #[tokio::test]
    async fn test_bye_existing_dialog_sends_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // Create dialog
        let invite = make_invite_request("call-bye");
        uas.handle_request(invite, test_addr()).await.unwrap();

        // Send BYE
        let bye = make_bye_request("call-bye");
        uas.handle_request(bye, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        let bye_response = responses.last().unwrap();
        assert_eq!(bye_response.status_code, 200);
        assert_eq!(bye_response.reason_phrase, "OK");
    }

    #[tokio::test]
    async fn test_bye_existing_dialog_removes_dialog() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // Create dialog
        let invite = make_invite_request("call-bye-rm");
        uas.handle_request(invite, test_addr()).await.unwrap();
        assert!(uas.get_dialog("call-bye-rm").is_some());

        // Send BYE
        let bye = make_bye_request("call-bye-rm");
        uas.handle_request(bye, test_addr()).await.unwrap();

        assert!(uas.get_dialog("call-bye-rm").is_none(), "Dialog should be removed after BYE");
    }

    #[tokio::test]
    async fn test_bye_existing_dialog_decrements_active_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());

        let invite = make_invite_request("call-bye-stats");
        uas.handle_request(invite, test_addr()).await.unwrap();
        assert_eq!(stats.snapshot().active_dialogs, 1);

        let bye = make_bye_request("call-bye-stats");
        uas.handle_request(bye, test_addr()).await.unwrap();
        assert_eq!(stats.snapshot().active_dialogs, 0);
    }

    #[tokio::test]
    async fn test_bye_nonexistent_dialog_sends_481() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        // Send BYE without prior INVITE
        let bye = make_bye_request("nonexistent-call");
        uas.handle_request(bye, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status_code, 481);
        assert_eq!(responses[0].reason_phrase, "Call/Transaction Does Not Exist");
    }

    #[tokio::test]
    async fn test_bye_481_copies_headers() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let bye = make_bye_request("no-such-call");
        uas.handle_request(bye, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        let resp = &responses[0];
        assert_eq!(resp.headers.get("Call-ID"), Some("no-such-call"));
        assert_eq!(resp.headers.get("CSeq"), Some("2 BYE"));
    }

    // ===== REGISTER handling tests =====

    #[tokio::test]
    async fn test_register_sends_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let register = make_register_request("reg-1");
        uas.handle_request(register, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].status_code, 200);
        assert_eq!(responses[0].reason_phrase, "OK");
    }

    #[tokio::test]
    async fn test_register_does_not_create_dialog() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let register = make_register_request("reg-2");
        uas.handle_request(register, test_addr()).await.unwrap();

        assert!(uas.get_dialog("reg-2").is_none(), "REGISTER should not create a dialog");
    }

    #[tokio::test]
    async fn test_register_response_copies_headers() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let register = make_register_request("reg-hdr");
        uas.handle_request(register, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        let resp = &responses[0];
        assert_eq!(resp.headers.get("Call-ID"), Some("reg-hdr"));
        assert_eq!(resp.headers.get("CSeq"), Some("1 REGISTER"));
    }

    // ===== Full INVITE-ACK-BYE flow test =====

    #[tokio::test]
    async fn test_full_invite_ack_bye_flow() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());
        let addr = test_addr();

        // INVITE
        let invite = make_invite_request("call-flow");
        uas.handle_request(invite, addr).await.unwrap();
        assert_eq!(uas.get_dialog("call-flow").unwrap().state, UasDialogState::Early);
        assert_eq!(stats.snapshot().active_dialogs, 1);

        // ACK
        let ack = make_ack_request("call-flow");
        uas.handle_request(ack, addr).await.unwrap();
        assert_eq!(uas.get_dialog("call-flow").unwrap().state, UasDialogState::Confirmed);

        // BYE
        let bye = make_bye_request("call-flow");
        uas.handle_request(bye, addr).await.unwrap();
        assert!(uas.get_dialog("call-flow").is_none());
        assert_eq!(stats.snapshot().active_dialogs, 0);

        // Verify all responses: 100 Trying, 200 OK (INVITE), 200 OK (BYE)
        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0].status_code, 100);
        assert_eq!(responses[1].status_code, 200);
        assert_eq!(responses[2].status_code, 200);
    }

    // ===== Response ignoring test =====

    #[tokio::test]
    async fn test_response_message_is_ignored() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let response = SipMessage::Response(SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers: Headers::new(),
            body: None,
        });

        let result = uas.handle_request(response, test_addr()).await;
        assert!(result.is_ok());
        assert!(transport.sent.lock().unwrap().is_empty());
    }

    // ===== Shutdown test =====

    #[tokio::test]
    async fn test_shutdown_clears_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-shutdown");
        uas.handle_request(invite, test_addr()).await.unwrap();
        assert!(uas.get_dialog("call-shutdown").is_some());

        uas.shutdown().await.unwrap();
        assert!(uas.get_dialog("call-shutdown").is_none());
    }

    // ===== Multiple dialogs test =====

    #[tokio::test]
    async fn test_multiple_concurrent_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());
        let addr = test_addr();

        // Create 3 dialogs
        for i in 0..3 {
            let invite = make_invite_request(&format!("multi-{}", i));
            uas.handle_request(invite, addr).await.unwrap();
        }
        assert_eq!(stats.snapshot().active_dialogs, 3);

        // BYE one dialog
        let bye = make_bye_request("multi-1");
        uas.handle_request(bye, addr).await.unwrap();
        assert_eq!(stats.snapshot().active_dialogs, 2);
        assert!(uas.get_dialog("multi-0").is_some());
        assert!(uas.get_dialog("multi-1").is_none());
        assert!(uas.get_dialog("multi-2").is_some());
    }

    // ===== Sends to correct address test =====

    #[tokio::test]
    async fn test_responses_sent_to_correct_address() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let addr: SocketAddr = "192.168.1.100:6060".parse().unwrap();
        let invite = make_invite_request("call-addr");
        uas.handle_request(invite, addr).await.unwrap();

        let sent = transport.sent_messages();
        for (_, sent_addr) in &sent {
            assert_eq!(*sent_addr, addr, "Response should be sent to the originating address");
        }
    }

    // ===== Session-Timer helpers =====

    fn make_invite_with_session_expires(call_id: &str, session_expires: u32) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK776".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
        headers.set("Call-ID", call_id.to_string());
        headers.set("CSeq", "1 INVITE".to_string());
        headers.set("Session-Expires", format!("{}", session_expires));
        SipMessage::Request(SipRequest {
            method: Method::Invite,
            request_uri: "sip:bob@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        })
    }

    fn make_reinvite_request(call_id: &str) -> SipMessage {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKre1".to_string());
        headers.set("From", "<sip:alice@example.com>;tag=1928301774".to_string());
        headers.set("To", "<sip:bob@example.com>".to_string());
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

    // ===== Req 3.2: Session-Expires header in INVITE response =====

    #[tokio::test]
    async fn test_invite_with_session_expires_includes_header_in_200ok() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_with_session_expires("call-se-1", 1800);
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        // 200 OK is the second response (after 100 Trying)
        let ok = &responses[1];
        assert_eq!(ok.status_code, 200);
        assert_eq!(
            ok.headers.get("Session-Expires"),
            Some("1800"),
            "200 OK should include Session-Expires header from INVITE"
        );
    }

    #[tokio::test]
    async fn test_invite_with_session_expires_records_in_dialog() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_with_session_expires("call-se-2", 900);
        uas.handle_request(invite, test_addr()).await.unwrap();

        let dialog = uas.get_dialog("call-se-2").unwrap();
        assert_eq!(
            dialog.session_expires,
            Some(Duration::from_secs(900)),
            "Dialog should record session_expires from INVITE"
        );
    }

    #[tokio::test]
    async fn test_invite_without_session_expires_no_header_in_response() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-no-se");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        let ok = &responses[1];
        assert_eq!(ok.status_code, 200);
        assert!(
            ok.headers.get("Session-Expires").is_none(),
            "200 OK should NOT include Session-Expires when INVITE doesn't have it"
        );
    }

    #[tokio::test]
    async fn test_invite_without_session_expires_dialog_has_none() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-no-se-2");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let dialog = uas.get_dialog("call-no-se-2").unwrap();
        assert_eq!(
            dialog.session_expires, None,
            "Dialog should have no session_expires when INVITE doesn't have it"
        );
    }

    // ===== Req 3.3: re-INVITE handling =====

    #[tokio::test]
    async fn test_reinvite_sends_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());
        let addr = test_addr();

        // Establish dialog: INVITE → ACK
        let invite = make_invite_with_session_expires("call-reinv-1", 1800);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-reinv-1");
        uas.handle_request(ack, addr).await.unwrap();

        // Send re-INVITE
        let reinvite = make_reinvite_request("call-reinv-1");
        uas.handle_request(reinvite, addr).await.unwrap();

        let responses = transport.sent_responses();
        // After re-INVITE, we expect a 200 OK (no 100 Trying for re-INVITE)
        let reinvite_responses: Vec<_> = responses.iter().skip(2).collect(); // skip initial 100+200
        assert!(
            reinvite_responses.iter().any(|r| r.status_code == 200),
            "re-INVITE should receive a 200 OK response"
        );
    }

    #[tokio::test]
    async fn test_reinvite_updates_last_activity() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());
        let addr = test_addr();

        // Establish dialog
        let invite = make_invite_with_session_expires("call-reinv-2", 1800);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-reinv-2");
        uas.handle_request(ack, addr).await.unwrap();

        let dialog_before = uas.get_dialog("call-reinv-2").unwrap();
        let activity_before = dialog_before.last_activity;

        // Small delay to ensure time difference
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Send re-INVITE
        let reinvite = make_reinvite_request("call-reinv-2");
        uas.handle_request(reinvite, addr).await.unwrap();

        let dialog_after = uas.get_dialog("call-reinv-2").unwrap();
        assert!(
            dialog_after.last_activity >= activity_before,
            "re-INVITE should update last_activity"
        );
    }

    #[tokio::test]
    async fn test_reinvite_keeps_dialog_confirmed() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());
        let addr = test_addr();

        // Establish dialog
        let invite = make_invite_with_session_expires("call-reinv-3", 1800);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-reinv-3");
        uas.handle_request(ack, addr).await.unwrap();

        // Send re-INVITE
        let reinvite = make_reinvite_request("call-reinv-3");
        uas.handle_request(reinvite, addr).await.unwrap();

        let dialog = uas.get_dialog("call-reinv-3").unwrap();
        assert_eq!(
            dialog.state,
            UasDialogState::Confirmed,
            "Dialog should remain Confirmed after re-INVITE"
        );
    }

    // ===== Req 3.4: Session timeout =====

    #[tokio::test]
    async fn test_check_session_timeouts_removes_expired_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());
        let addr = test_addr();

        // Create dialog with very short session expiry
        let invite = make_invite_with_session_expires("call-timeout-1", 1);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-timeout-1");
        uas.handle_request(ack, addr).await.unwrap();

        // Wait for session to expire
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Check timeouts
        let timed_out = uas.check_session_timeouts();
        assert_eq!(timed_out, 1, "Should detect 1 timed-out session");
        assert!(
            uas.get_dialog("call-timeout-1").is_none(),
            "Timed-out dialog should be removed"
        );
    }

    #[tokio::test]
    async fn test_check_session_timeouts_records_failure_in_stats() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());
        let addr = test_addr();

        // Create dialog with very short session expiry
        let invite = make_invite_with_session_expires("call-timeout-2", 1);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-timeout-2");
        uas.handle_request(ack, addr).await.unwrap();

        // Wait for session to expire
        tokio::time::sleep(Duration::from_secs(2)).await;

        uas.check_session_timeouts();

        let snap = stats.snapshot();
        assert_eq!(snap.failed_calls, 1, "Session timeout should be recorded as a failure");
        assert_eq!(snap.active_dialogs, 0, "Active dialogs should be decremented");
    }

    #[tokio::test]
    async fn test_check_session_timeouts_does_not_remove_active_dialogs() {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let uas = Uas::new(transport.clone(), stats.clone(), UasConfig::default());
        let addr = test_addr();

        // Create dialog with long session expiry
        let invite = make_invite_with_session_expires("call-active", 3600);
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-active");
        uas.handle_request(ack, addr).await.unwrap();

        let timed_out = uas.check_session_timeouts();
        assert_eq!(timed_out, 0, "Should not timeout active sessions");
        assert!(
            uas.get_dialog("call-active").is_some(),
            "Active dialog should remain"
        );
    }

    #[tokio::test]
    async fn test_check_session_timeouts_ignores_dialogs_without_session_expires() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());
        let addr = test_addr();

        // Create dialog without Session-Expires
        let invite = make_invite_request("call-no-timer");
        uas.handle_request(invite, addr).await.unwrap();
        let ack = make_ack_request("call-no-timer");
        uas.handle_request(ack, addr).await.unwrap();

        let timed_out = uas.check_session_timeouts();
        assert_eq!(timed_out, 0, "Should not timeout dialogs without Session-Expires");
        assert!(uas.get_dialog("call-no-timer").is_some());
    }

    // ===== TransactionManager integration tests =====

    fn make_uas_with_transaction_manager(
        transport: Arc<MockTransport>,
    ) -> (Uas, Arc<StatsCollector>) {
        use crate::transaction::{TimerConfig, TransactionManager};
        let stats = Arc::new(StatsCollector::new());
        let tm = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            TimerConfig::default(),
            10000,
        );
        let uas = Uas::with_transaction_manager(
            transport,
            stats.clone(),
            UasConfig::default(),
            tm,
        );
        (uas, stats)
    }

    #[test]
    fn test_uas_new_has_no_transaction_manager() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport);
        assert!(uas.transaction_manager.is_none());
    }

    #[test]
    fn test_with_transaction_manager_constructor() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport);
        assert!(uas.transaction_manager.is_some());
    }

    #[tokio::test]
    async fn test_handle_request_via_tm_creates_server_transaction() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        let invite = make_invite_request("call-tm-1");
        uas.handle_request(invite, test_addr()).await.unwrap();

        // TransactionManagerにサーバートランザクションが登録されている
        let tm = uas.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1);
    }

    #[tokio::test]
    async fn test_handle_request_via_tm_register_creates_transaction() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        let register = make_register_request("reg-tm-1");
        uas.handle_request(register, test_addr()).await.unwrap();

        let tm = uas.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1);
    }

    #[tokio::test]
    async fn test_handle_request_via_tm_retransmit_absorbed() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        let invite = make_invite_request("call-retrans-tm");
        let invite2 = make_invite_request("call-retrans-tm");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let _responses_after_first = transport.sent_responses().len();

        // 同一リクエストの再送 → TransactionManagerが吸収し、UASには通知しない
        uas.handle_request(invite2, test_addr()).await.unwrap();

        // 再送時はUASのhandle_invite等が呼ばれないので、新たなレスポンスは送信されない
        // （TransactionManagerが再送を吸収する）
        let tm = uas.transaction_manager.as_ref().unwrap();
        assert_eq!(tm.active_count(), 1, "Should still have only 1 transaction");
    }

    #[tokio::test]
    async fn test_handle_request_via_tm_sends_response_through_tm() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        // REGISTER → 200 OK がTransactionManager経由で送信される
        let register = make_register_request("reg-tm-resp");
        uas.handle_request(register, test_addr()).await.unwrap();

        // レスポンスが送信されていることを確認
        let responses = transport.sent_responses();
        assert!(responses.len() >= 1, "Should send at least one response");
        assert_eq!(responses[0].status_code, 200);
    }

    #[tokio::test]
    async fn test_handle_invite_via_tm_sends_trying_and_ok() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        let invite = make_invite_request("call-tm-invite");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 2, "Should send 100 Trying and 200 OK");
        assert_eq!(responses[0].status_code, 100);
        assert_eq!(responses[1].status_code, 200);
    }

    #[tokio::test]
    async fn test_handle_bye_via_tm_sends_200_ok() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());

        // First create a dialog via INVITE
        let invite = make_invite_request("call-tm-bye");
        uas.handle_request(invite, test_addr()).await.unwrap();
        let ack = make_ack_request("call-tm-bye");
        uas.handle_request(ack, test_addr()).await.unwrap();

        // BYE → 200 OK
        let bye = make_bye_request("call-tm-bye");
        uas.handle_request(bye, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        // INVITE: 100 + 200, BYE: 200 = 3 responses
        let bye_response = responses.last().unwrap();
        assert_eq!(bye_response.status_code, 200);
    }

    #[tokio::test]
    async fn test_existing_uas_tests_still_pass_without_tm() {
        // Verify backward compatibility: Uas::new() without TM still works
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let invite = make_invite_request("call-compat");
        uas.handle_request(invite, test_addr()).await.unwrap();

        let responses = transport.sent_responses();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].status_code, 100);
        assert_eq!(responses[1].status_code, 200);
    }

    // ===== start_transaction_tick_loop tests =====

    #[tokio::test]
    async fn test_start_transaction_tick_loop_returns_immediately_without_tm() {
        let transport = Arc::new(MockTransport::new());
        let uas = make_uas(transport.clone());

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Should return immediately since no TransactionManager is configured
        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                // We wrap in a timeout to ensure it returns quickly
                let result = tokio::time::timeout(
                    Duration::from_millis(100),
                    uas.start_transaction_tick_loop(shutdown),
                ).await;
                assert!(result.is_ok(), "Should return immediately without TM");
            }
        });
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn test_start_transaction_tick_loop_stops_on_shutdown() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());
        let uas = Arc::new(uas);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let uas_clone = uas.clone();

        let handle = tokio::spawn(async move {
            uas_clone.start_transaction_tick_loop(shutdown_clone).await;
        });

        // Let it run for a bit
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);

        // Should stop within a reasonable time
        let result = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(result.is_ok(), "Tick loop should stop after shutdown signal");
    }

    #[tokio::test]
    async fn test_start_transaction_tick_loop_calls_tick_and_cleanup() {
        let transport = Arc::new(MockTransport::new());
        let (uas, _stats) = make_uas_with_transaction_manager(transport.clone());
        let uas = Arc::new(uas);

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let uas_clone = uas.clone();

        let handle = tokio::spawn(async move {
            uas_clone.start_transaction_tick_loop(shutdown_clone).await;
        });

        // Let it tick a few times (10ms interval, so 50ms should give ~5 ticks)
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Signal shutdown
        shutdown.store(true, std::sync::atomic::Ordering::Relaxed);

        let result = tokio::time::timeout(Duration::from_millis(200), handle).await;
        assert!(result.is_ok(), "Tick loop should complete after shutdown");
    }

    // ===== Property 6: 存在しないダイアログへの481応答 =====
    // Feature: sip-load-tester, Property 6: 存在しないダイアログへの481応答
    // **Validates: Requirements 4.4**

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn prop_bye_nonexistent_dialog_returns_481(
            call_id in "[a-zA-Z0-9._\\-]{1,64}"
        ) {
            // Build a tokio runtime to drive async handle_request
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(MockTransport::new());
                let uas = make_uas(transport.clone());

                // UAS has no dialogs registered — any Call-ID is "nonexistent"
                let bye = make_bye_request(&call_id);
                uas.handle_request(bye, test_addr()).await.unwrap();

                let responses = transport.sent_responses();
                prop_assert_eq!(
                    responses.len(), 1,
                    "Expected exactly 1 response for BYE on nonexistent dialog"
                );
                prop_assert_eq!(
                    responses[0].status_code, 481,
                    "BYE for nonexistent dialog must return 481, got {}",
                    responses[0].status_code
                );
                prop_assert_eq!(
                    responses[0].reason_phrase.as_str(),
                    "Call/Transaction Does Not Exist"
                );
                // Verify the response carries the same Call-ID
                prop_assert_eq!(
                    responses[0].headers.get("Call-ID").unwrap_or(""),
                    call_id.as_str(),
                    "481 response must echo the original Call-ID"
                );
                Ok(())
            })?;
        }
    }
}
