use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::sip::message::{SipRequest, SipResponse};

use super::{TimerConfig, TransactionId};

/// サーバートランザクション（INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteServerState {
    Proceeding,
    Completed,
    Confirmed,
    Terminated,
}

/// INVITEサーバートランザクションのアクション
#[derive(Debug, Clone, PartialEq)]
pub enum InviteServerAction {
    /// アクション不要（既にTerminated、またはイベント無視）
    None,
    /// 暫定レスポンス再送（INVITE再受信時）
    SendProvisionalResponse(SipResponse),
    /// レスポンス送信完了（send_response用）
    SendResponse,
    /// 最終レスポンス再送（Timer_G発火時）
    RetransmitResponse(SipResponse),
    /// トランザクションタイムアウト（Timer_H発火、ACK未受信）
    Timeout,
}

/// INVITEサーバートランザクション
pub struct InviteServerTransaction {
    pub id: TransactionId,
    pub state: InviteServerState,
    pub original_request: SipRequest,
    pub last_provisional_response: Option<SipResponse>,
    pub last_final_response: Option<SipResponse>,
    pub source: SocketAddr,
    /// Timer_G: レスポンス再送タイマー（次回発火時刻）
    pub timer_g_next: Option<Instant>,
    /// Timer_G: 現在の再送間隔
    pub timer_g_interval: Duration,
    /// Timer_H: ACK待機タイマー期限
    pub timer_h_deadline: Option<Instant>,
    /// Timer_I: Confirmed状態待機タイマー期限
    pub timer_i_deadline: Option<Instant>,
    pub retransmit_count: u32,
}

impl InviteServerTransaction {
    /// Proceeding状態で新しいINVITEサーバートランザクションを作成
    /// リクエストを保持する
    pub fn new(
        id: TransactionId,
        request: SipRequest,
        source: SocketAddr,
        timer_config: &TimerConfig,
    ) -> Self {
        Self {
            id,
            state: InviteServerState::Proceeding,
            original_request: request,
            last_provisional_response: None,
            last_final_response: None,
            source,
            timer_g_next: None,
            timer_g_interval: timer_config.t1,
            timer_h_deadline: None,
            timer_i_deadline: None,
            retransmit_count: 0,
        }
    }

    /// INVITE再受信時の処理
    /// Proceeding状態で暫定レスポンスを再送する
    pub fn on_request_retransmit(&self) -> InviteServerAction {
        if self.state != InviteServerState::Proceeding {
            return InviteServerAction::None;
        }
        match &self.last_provisional_response {
            Some(resp) => InviteServerAction::SendProvisionalResponse(resp.clone()),
            None => InviteServerAction::None,
        }
    }

    /// レスポンス送信指示
    /// 2xx → Terminated、3xx-6xx → Completed + Timer_G/Timer_H開始
    pub fn send_response(&mut self, status_code: u16, response: SipResponse, timer_config: &TimerConfig) -> InviteServerAction {
        if self.state != InviteServerState::Proceeding {
            return InviteServerAction::None;
        }
        if (200..300).contains(&status_code) {
            // 2xx → Terminated
            self.state = InviteServerState::Terminated;
            InviteServerAction::SendResponse
        } else if (300..700).contains(&status_code) {
            // 3xx-6xx → Completed, Timer_G(T1)/Timer_H(64*T1)開始
            self.state = InviteServerState::Completed;
            self.last_final_response = Some(response);
            let now = Instant::now();
            self.timer_g_next = Some(now + timer_config.t1);
            self.timer_g_interval = timer_config.t1;
            self.timer_h_deadline = Some(now + timer_config.timer_h());
            InviteServerAction::SendResponse
        } else {
            InviteServerAction::None
        }
    }

    /// Timer_G発火時の処理（レスポンス再送）
    /// Completed状態でレスポンスを再送し、Timer_G間隔をmin(2*interval, T2)に更新
    pub fn on_timer_g(&mut self, timer_config: &TimerConfig) -> InviteServerAction {
        if self.state != InviteServerState::Completed {
            return InviteServerAction::None;
        }
        let resp = match &self.last_final_response {
            Some(r) => r.clone(),
            None => return InviteServerAction::None,
        };
        // 間隔を倍増し、T2で上限
        self.timer_g_interval = (self.timer_g_interval * 2).min(timer_config.t2);
        self.timer_g_next = Some(Instant::now() + self.timer_g_interval);
        self.retransmit_count += 1;
        InviteServerAction::RetransmitResponse(resp)
    }

    /// Timer_H発火時の処理（ACK未受信タイムアウト）
    /// Completed状態からTerminated状態に遷移
    pub fn on_timer_h(&mut self) -> InviteServerAction {
        if self.state != InviteServerState::Completed {
            return InviteServerAction::None;
        }
        self.state = InviteServerState::Terminated;
        InviteServerAction::Timeout
    }

    /// ACK受信時の処理
    /// Completed状態からConfirmed状態に遷移、Timer_I開始
    pub fn on_ack_received(&mut self, timer_config: &TimerConfig) -> InviteServerAction {
        if self.state != InviteServerState::Completed {
            return InviteServerAction::None;
        }
        self.state = InviteServerState::Confirmed;
        self.timer_i_deadline = Some(Instant::now() + timer_config.timer_i());
        // Timer_G/H停止
        self.timer_g_next = None;
        self.timer_h_deadline = None;
        InviteServerAction::None
    }

    /// Timer_I発火時の処理
    /// Confirmed状態からTerminated状態に遷移
    pub fn on_timer_i(&mut self) -> InviteServerAction {
        if self.state != InviteServerState::Confirmed {
            return InviteServerAction::None;
        }
        self.state = InviteServerState::Terminated;
        InviteServerAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method};
    use std::net::SocketAddr;

    fn make_request(method: Method, branch: &str) -> SipRequest {
        let mut headers = Headers::new();
        headers.add("Via", format!("SIP/2.0/UDP 192.168.1.1:5060;branch={}", branch));
        headers.add("CSeq", format!("1 {}", method));
        SipRequest {
            method,
            request_uri: "sip:test@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        }
    }

    fn make_response(status_code: u16, branch: &str, method: &str) -> SipResponse {
        let mut headers = Headers::new();
        headers.add("Via", format!("SIP/2.0/UDP 192.168.1.1:5060;branch={}", branch));
        headers.add("CSeq", format!("1 {}", method));
        SipResponse {
            version: "SIP/2.0".to_string(),
            status_code,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        }
    }

    fn make_invite_server_transaction(branch: &str) -> InviteServerTransaction {
        let req = make_request(Method::Invite, branch);
        let id = TransactionId::from_request(&req).unwrap();
        InviteServerTransaction {
            id,
            state: InviteServerState::Proceeding,
            original_request: req,
            last_provisional_response: None,
            last_final_response: None,
            source: "192.168.1.1:5060".parse().unwrap(),
            timer_g_next: None,
            timer_g_interval: TimerConfig::default().t1,
            timer_h_deadline: None,
            timer_i_deadline: None,
            retransmit_count: 0,
        }
    }

    #[test]
    fn invite_server_new_creates_proceeding_state() {
        let req = make_request(Method::Invite, "z9hG4bKisrv001");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let config = TimerConfig::default();
        let tx = InviteServerTransaction::new(id, req, source, &config);

        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(tx.id.method, Method::Invite);
        assert!(tx.last_provisional_response.is_none());
        assert!(tx.last_final_response.is_none());
        assert!(tx.timer_g_next.is_none());
        assert!(tx.timer_h_deadline.is_none());
        assert!(tx.timer_i_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
        assert_eq!(tx.timer_g_interval, config.t1);
    }

    #[test]
    fn invite_server_on_request_retransmit_in_proceeding_with_provisional() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv002");
        let provisional = make_response(100, "z9hG4bKisrv002", "INVITE");
        tx.last_provisional_response = Some(provisional.clone());

        let action = tx.on_request_retransmit();
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(action, InviteServerAction::SendProvisionalResponse(provisional));
    }

    #[test]
    fn invite_server_on_request_retransmit_in_proceeding_without_provisional() {
        let tx = make_invite_server_transaction("z9hG4bKisrv003");
        assert!(tx.last_provisional_response.is_none());

        let action = tx.on_request_retransmit();
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_request_retransmit_in_non_proceeding_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv004");
        tx.state = InviteServerState::Completed;
        let action = tx.on_request_retransmit();
        assert_eq!(action, InviteServerAction::None);

        tx.state = InviteServerState::Confirmed;
        let action = tx.on_request_retransmit();
        assert_eq!(action, InviteServerAction::None);

        tx.state = InviteServerState::Terminated;
        let action = tx.on_request_retransmit();
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_send_response_2xx_transitions_to_terminated() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv005");
        let config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKisrv005", "INVITE");

        let action = tx.send_response(200, resp, &config);
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::SendResponse);
        // 2xx → Terminated: Timer_G/H should NOT be started
        assert!(tx.timer_g_next.is_none());
        assert!(tx.timer_h_deadline.is_none());
    }

    #[test]
    fn invite_server_send_response_3xx_transitions_to_completed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv006");
        let config = TimerConfig::default();
        let resp = make_response(300, "z9hG4bKisrv006", "INVITE");

        let action = tx.send_response(300, resp.clone(), &config);
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::SendResponse);
        assert!(tx.timer_g_next.is_some());
        assert!(tx.timer_h_deadline.is_some());
        assert_eq!(tx.last_final_response, Some(resp));
    }

    #[test]
    fn invite_server_send_response_4xx_transitions_to_completed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv007");
        let config = TimerConfig::default();
        let resp = make_response(486, "z9hG4bKisrv007", "INVITE");

        let action = tx.send_response(486, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::SendResponse);
        assert!(tx.timer_g_next.is_some());
        assert!(tx.timer_h_deadline.is_some());
    }

    #[test]
    fn invite_server_send_response_6xx_transitions_to_completed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv008");
        let config = TimerConfig::default();
        let resp = make_response(603, "z9hG4bKisrv008", "INVITE");

        let action = tx.send_response(603, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::SendResponse);
    }

    #[test]
    fn invite_server_send_response_timer_g_initial_interval_is_t1() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv009");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv009", "INVITE");

        tx.send_response(400, resp, &config);
        assert_eq!(tx.timer_g_interval, config.t1);
    }

    #[test]
    fn invite_server_send_response_timer_h_deadline_is_64_times_t1() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv010");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv010", "INVITE");
        let before = Instant::now();

        tx.send_response(400, resp, &config);
        let after = Instant::now();

        let deadline = tx.timer_h_deadline.unwrap();
        // Timer_H = 64 * T1
        assert!(deadline >= before + config.timer_h());
        assert!(deadline <= after + config.timer_h());
    }

    #[test]
    fn invite_server_send_response_not_in_proceeding_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv011");
        let config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKisrv011", "INVITE");

        tx.state = InviteServerState::Completed;
        let action = tx.send_response(200, resp.clone(), &config);
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::None);

        tx.state = InviteServerState::Terminated;
        let action = tx.send_response(200, resp, &config);
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_timer_g_retransmits_in_completed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv012");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv012", "INVITE");

        tx.send_response(400, resp.clone(), &config);
        assert_eq!(tx.state, InviteServerState::Completed);

        let action = tx.on_timer_g(&config);
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::RetransmitResponse(resp));
        assert_eq!(tx.retransmit_count, 1);
    }

    #[test]
    fn invite_server_on_timer_g_doubles_interval_capped_by_t2() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv013");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv013", "INVITE");

        tx.send_response(400, resp, &config);

        // 1st Timer_G: interval = min(2*T1, T2) = 1000ms
        tx.on_timer_g(&config);
        assert_eq!(tx.timer_g_interval, Duration::from_millis(1000));

        // 2nd Timer_G: interval = min(2*1000ms, T2) = 2000ms
        tx.on_timer_g(&config);
        assert_eq!(tx.timer_g_interval, Duration::from_millis(2000));

        // 3rd Timer_G: interval = min(2*2000ms, T2=4s) = 4000ms = T2
        tx.on_timer_g(&config);
        assert_eq!(tx.timer_g_interval, Duration::from_millis(4000));

        // 4th Timer_G: interval = min(2*4000ms, T2=4s) = 4000ms = T2 (capped)
        tx.on_timer_g(&config);
        assert_eq!(tx.timer_g_interval, Duration::from_millis(4000));
    }

    #[test]
    fn invite_server_on_timer_g_not_in_completed_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv014");
        let config = TimerConfig::default();

        // Proceeding state
        let action = tx.on_timer_g(&config);
        assert_eq!(action, InviteServerAction::None);

        // Confirmed state
        tx.state = InviteServerState::Confirmed;
        let action = tx.on_timer_g(&config);
        assert_eq!(action, InviteServerAction::None);

        // Terminated state
        tx.state = InviteServerState::Terminated;
        let action = tx.on_timer_g(&config);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_timer_h_terminates_from_completed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv015");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv015", "INVITE");

        tx.send_response(400, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);

        let action = tx.on_timer_h();
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::Timeout);
    }

    #[test]
    fn invite_server_on_timer_h_not_in_completed_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv016");

        // Proceeding state
        let action = tx.on_timer_h();
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(action, InviteServerAction::None);

        // Confirmed state
        tx.state = InviteServerState::Confirmed;
        let action = tx.on_timer_h();
        assert_eq!(tx.state, InviteServerState::Confirmed);
        assert_eq!(action, InviteServerAction::None);

        // Terminated state
        tx.state = InviteServerState::Terminated;
        let action = tx.on_timer_h();
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_ack_received_transitions_to_confirmed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv017");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv017", "INVITE");

        tx.send_response(400, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);

        let action = tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Confirmed);
        assert_eq!(action, InviteServerAction::None);
        assert!(tx.timer_i_deadline.is_some());
    }

    #[test]
    fn invite_server_on_ack_received_timer_i_is_t4() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv018");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv018", "INVITE");

        tx.send_response(400, resp, &config);
        let before = Instant::now();
        tx.on_ack_received(&config);
        let after = Instant::now();

        let deadline = tx.timer_i_deadline.unwrap();
        assert!(deadline >= before + config.timer_i());
        assert!(deadline <= after + config.timer_i());
    }

    #[test]
    fn invite_server_on_ack_received_not_in_completed_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv019");
        let config = TimerConfig::default();

        // Proceeding state
        let action = tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(action, InviteServerAction::None);

        // Confirmed state
        tx.state = InviteServerState::Confirmed;
        let action = tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Confirmed);
        assert_eq!(action, InviteServerAction::None);

        // Terminated state
        tx.state = InviteServerState::Terminated;
        let action = tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_timer_i_terminates_from_confirmed() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv020");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv020", "INVITE");

        tx.send_response(400, resp, &config);
        tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Confirmed);

        let action = tx.on_timer_i();
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_on_timer_i_not_in_confirmed_ignored() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv021");

        // Proceeding state
        let action = tx.on_timer_i();
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(action, InviteServerAction::None);

        // Completed state
        tx.state = InviteServerState::Completed;
        let action = tx.on_timer_i();
        assert_eq!(tx.state, InviteServerState::Completed);
        assert_eq!(action, InviteServerAction::None);

        // Terminated state
        tx.state = InviteServerState::Terminated;
        let action = tx.on_timer_i();
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::None);
    }

    #[test]
    fn invite_server_terminated_ignores_all_events() {
        let mut tx = make_invite_server_transaction("z9hG4bKisrv022");
        let config = TimerConfig::default();
        tx.state = InviteServerState::Terminated;

        assert_eq!(tx.on_request_retransmit(), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);

        let resp = make_response(200, "z9hG4bKisrv022", "INVITE");
        assert_eq!(tx.send_response(200, resp, &config), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);

        assert_eq!(tx.on_timer_g(&config), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);

        assert_eq!(tx.on_timer_h(), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);

        assert_eq!(tx.on_ack_received(&config), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);

        assert_eq!(tx.on_timer_i(), InviteServerAction::None);
        assert_eq!(tx.state, InviteServerState::Terminated);
    }

    #[test]
    fn invite_server_full_3xx_6xx_lifecycle() {
        // Proceeding → send 400 → Completed → ACK → Confirmed → Timer_I → Terminated
        let mut tx = make_invite_server_transaction("z9hG4bKisrv023");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv023", "INVITE");

        assert_eq!(tx.state, InviteServerState::Proceeding);

        tx.send_response(400, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);

        tx.on_ack_received(&config);
        assert_eq!(tx.state, InviteServerState::Confirmed);

        tx.on_timer_i();
        assert_eq!(tx.state, InviteServerState::Terminated);
    }

    #[test]
    fn invite_server_full_2xx_lifecycle() {
        // Proceeding → send 200 → Terminated
        let mut tx = make_invite_server_transaction("z9hG4bKisrv024");
        let config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKisrv024", "INVITE");

        assert_eq!(tx.state, InviteServerState::Proceeding);

        tx.send_response(200, resp, &config);
        assert_eq!(tx.state, InviteServerState::Terminated);
    }

    #[test]
    fn invite_server_timer_h_timeout_lifecycle() {
        // Proceeding → send 400 → Completed → Timer_H → Terminated
        let mut tx = make_invite_server_transaction("z9hG4bKisrv025");
        let config = TimerConfig::default();
        let resp = make_response(400, "z9hG4bKisrv025", "INVITE");

        tx.send_response(400, resp, &config);
        assert_eq!(tx.state, InviteServerState::Completed);

        let action = tx.on_timer_h();
        assert_eq!(tx.state, InviteServerState::Terminated);
        assert_eq!(action, InviteServerAction::Timeout);
    }

    // === Property-based tests ===

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// INVITEサーバートランザクションに適用するイベント
        #[derive(Debug, Clone)]
        enum InviteServerEvent {
            RequestRetransmit,
            SendResponse(u16),
            TimerG,
            TimerH,
            AckReceived,
            TimerI,
        }

        /// ランダムなINVITEサーバーイベントを生成するストラテジー
        fn arb_invite_server_event() -> impl Strategy<Value = InviteServerEvent> {
            prop_oneof![
                Just(InviteServerEvent::RequestRetransmit),
                (200u16..700).prop_map(InviteServerEvent::SendResponse),
                Just(InviteServerEvent::TimerG),
                Just(InviteServerEvent::TimerH),
                Just(InviteServerEvent::AckReceived),
                Just(InviteServerEvent::TimerI),
            ]
        }

        /// INVITEサーバートランザクションの状態遷移テーブル
        fn expected_invite_server_next_state(
            current: InviteServerState,
            event: &InviteServerEvent,
        ) -> InviteServerState {
            match (current, event) {
                // Proceeding状態
                (InviteServerState::Proceeding, InviteServerEvent::RequestRetransmit) => {
                    InviteServerState::Proceeding
                }
                (InviteServerState::Proceeding, InviteServerEvent::SendResponse(code)) => {
                    if (200..300).contains(code) {
                        InviteServerState::Terminated
                    } else if (300..700).contains(code) {
                        InviteServerState::Completed
                    } else {
                        InviteServerState::Proceeding
                    }
                }
                (InviteServerState::Proceeding, _) => {
                    InviteServerState::Proceeding // Timer_G/H/I, ACK have no effect in Proceeding
                }

                // Completed状態
                (InviteServerState::Completed, InviteServerEvent::TimerG) => {
                    InviteServerState::Completed // retransmit response, stay Completed
                }
                (InviteServerState::Completed, InviteServerEvent::AckReceived) => {
                    InviteServerState::Confirmed
                }
                (InviteServerState::Completed, InviteServerEvent::TimerH) => {
                    InviteServerState::Terminated
                }
                (InviteServerState::Completed, _) => {
                    InviteServerState::Completed // Other events ignored in Completed
                }

                // Confirmed状態
                (InviteServerState::Confirmed, InviteServerEvent::TimerI) => {
                    InviteServerState::Terminated
                }
                (InviteServerState::Confirmed, _) => {
                    InviteServerState::Confirmed // Other events ignored in Confirmed
                }

                // Terminated状態 — すべてのイベントを無視
                (InviteServerState::Terminated, _) => InviteServerState::Terminated,
            }
        }

        /// INVITEサーバーイベントをトランザクションに適用する
        fn apply_invite_server_event(
            tx: &mut InviteServerTransaction,
            event: &InviteServerEvent,
            timer_config: &TimerConfig,
        ) {
            match event {
                InviteServerEvent::RequestRetransmit => {
                    tx.on_request_retransmit();
                }
                InviteServerEvent::SendResponse(code) => {
                    let resp = make_response(*code, &tx.id.branch, "INVITE");
                    tx.send_response(*code, resp, timer_config);
                }
                InviteServerEvent::TimerG => {
                    tx.on_timer_g(timer_config);
                }
                InviteServerEvent::TimerH => {
                    tx.on_timer_h();
                }
                InviteServerEvent::AckReceived => {
                    tx.on_ack_received(timer_config);
                }
                InviteServerEvent::TimerI => {
                    tx.on_timer_i();
                }
            }
        }

        // Feature: sip-transaction-layer, Property 5: INVITEサーバートランザクション状態遷移の正当性
        // **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.6, 4.7, 4.8**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn invite_server_state_transitions_match_table(
                events in proptest::collection::vec(arb_invite_server_event(), 1..20),
            ) {
                let timer_config = TimerConfig::default();
                let req = make_request(Method::Invite, "z9hG4bKproptest_is");
                let id = TransactionId::from_request(&req).unwrap();
                let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
                let mut tx = InviteServerTransaction::new(id, req, source, &timer_config);

                for event in &events {
                    let expected = expected_invite_server_next_state(tx.state, event);
                    apply_invite_server_event(&mut tx, event, &timer_config);
                    prop_assert_eq!(
                        tx.state,
                        expected,
                        "State mismatch after event {:?}: expected {:?}, got {:?}",
                        event,
                        expected,
                        tx.state,
                    );
                }
            }
        }

        // Feature: sip-transaction-layer, Property 12: Timer_G再送間隔の上限
        // **Validates: Requirements 4.5, 6.5**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn timer_g_interval_never_exceeds_t2(
                t1_ms in 1u64..=2000,
                t2_ms in 1000u64..=10000,
                n in 0u32..=20,
            ) {
                let t1 = Duration::from_millis(t1_ms);
                let t2 = Duration::from_millis(t2_ms);
                let timer_config = TimerConfig {
                    t1,
                    t2,
                    t4: Duration::from_secs(5),
                };
                let req = make_request(Method::Invite, "z9hG4bKtimerG");
                let id = TransactionId::from_request(&req).unwrap();
                let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
                let mut tx = InviteServerTransaction::new(id, req, source, &timer_config);

                // 3xx-6xxレスポンスを送信してCompleted状態に遷移
                let resp = make_response(400, "z9hG4bKtimerG", "INVITE");
                tx.send_response(400, resp, &timer_config);
                prop_assert_eq!(tx.state, InviteServerState::Completed);

                // 初期状態: timer_g_interval == T1
                prop_assert_eq!(tx.timer_g_interval, t1);

                // n回 on_timer_g() を呼び出し、毎回 interval <= T2 を検証
                for i in 0..n {
                    tx.on_timer_g(&timer_config);
                    prop_assert!(
                        tx.timer_g_interval <= t2,
                        "After {} retransmissions with T1={:?}, T2={:?}: interval {:?} exceeds T2",
                        i + 1,
                        t1,
                        t2,
                        tx.timer_g_interval,
                    );
                }
            }
        }
    }
}
