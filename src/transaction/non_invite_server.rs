use std::net::SocketAddr;
use std::time::Instant;

use crate::sip::message::{SipRequest, SipResponse};

use super::{TimerConfig, TransactionId};

/// サーバートランザクション（Non-INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInviteServerState {
    Trying,
    Proceeding,
    Completed,
    Terminated,
}

/// Non-INVITEサーバートランザクションのアクション
#[derive(Debug, Clone, PartialEq)]
pub enum NonInviteServerAction {
    /// アクション不要（吸収、無視、またはTerminated状態）
    None,
    /// 暫定レスポンス再送（Proceeding状態でリクエスト再受信時）
    SendProvisionalResponse(SipResponse),
    /// 最終レスポンス再送（Completed状態でリクエスト再受信時）
    SendFinalResponse(SipResponse),
    /// レスポンス送信完了（send_response用）
    SendResponse,
}

/// Non-INVITEサーバートランザクション
pub struct NonInviteServerTransaction {
    pub id: TransactionId,
    pub state: NonInviteServerState,
    pub original_request: SipRequest,
    pub last_provisional_response: Option<SipResponse>,
    pub last_final_response: Option<SipResponse>,
    pub source: SocketAddr,
    /// Timer_J: Completed状態待機タイマー期限
    pub timer_j_deadline: Option<Instant>,
    pub retransmit_count: u32,
}

impl NonInviteServerTransaction {
    /// Trying状態で新しいNon-INVITEサーバートランザクションを作成
    /// リクエストを保持する
    pub fn new(
        id: TransactionId,
        request: SipRequest,
        source: SocketAddr,
    ) -> Self {
        Self {
            id,
            state: NonInviteServerState::Trying,
            original_request: request,
            last_provisional_response: None,
            last_final_response: None,
            source,
            timer_j_deadline: None,
            retransmit_count: 0,
        }
    }

    /// リクエスト再受信時の処理
    /// Trying状態: 吸収（上位層に通知しない）
    /// Proceeding状態: 暫定レスポンス再送
    /// Completed状態: 最終レスポンス再送
    pub fn on_request_retransmit(&mut self) -> NonInviteServerAction {
        match self.state {
            NonInviteServerState::Trying => {
                // 吸収 — 上位層に通知しない
                NonInviteServerAction::None
            }
            NonInviteServerState::Proceeding => {
                match &self.last_provisional_response {
                    Some(resp) => {
                        self.retransmit_count += 1;
                        NonInviteServerAction::SendProvisionalResponse(resp.clone())
                    }
                    None => NonInviteServerAction::None,
                }
            }
            NonInviteServerState::Completed => {
                match &self.last_final_response {
                    Some(resp) => {
                        self.retransmit_count += 1;
                        NonInviteServerAction::SendFinalResponse(resp.clone())
                    }
                    None => NonInviteServerAction::None,
                }
            }
            NonInviteServerState::Terminated => NonInviteServerAction::None,
        }
    }

    /// レスポンス送信指示
    /// 1xx → Proceeding、2xx-6xx → Completed + Timer_J(64*T1)開始
    pub fn send_response(&mut self, status_code: u16, response: SipResponse, timer_config: &TimerConfig) -> NonInviteServerAction {
        match self.state {
            NonInviteServerState::Trying | NonInviteServerState::Proceeding => {
                if (100..200).contains(&status_code) {
                    // 1xx → Proceeding
                    self.state = NonInviteServerState::Proceeding;
                    self.last_provisional_response = Some(response);
                    NonInviteServerAction::SendResponse
                } else if (200..700).contains(&status_code) {
                    // 2xx-6xx → Completed + Timer_J開始
                    self.state = NonInviteServerState::Completed;
                    self.last_final_response = Some(response);
                    self.timer_j_deadline = Some(Instant::now() + timer_config.timer_j());
                    NonInviteServerAction::SendResponse
                } else {
                    NonInviteServerAction::None
                }
            }
            _ => NonInviteServerAction::None,
        }
    }

    /// Timer_J発火時の処理
    /// Completed状態からTerminated状態に遷移
    pub fn on_timer_j(&mut self) -> NonInviteServerAction {
        if self.state != NonInviteServerState::Completed {
            return NonInviteServerAction::None;
        }
        self.state = NonInviteServerState::Terminated;
        NonInviteServerAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::{Headers, Method};

    // === Helper functions ===

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

    fn make_non_invite_server_transaction(branch: &str) -> NonInviteServerTransaction {
        let req = make_request(Method::Register, branch);
        let id = TransactionId::from_request(&req).unwrap();
        NonInviteServerTransaction {
            id,
            state: NonInviteServerState::Trying,
            original_request: req,
            last_provisional_response: None,
            last_final_response: None,
            source: "192.168.1.1:5060".parse().unwrap(),
            timer_j_deadline: None,
            retransmit_count: 0,
        }
    }

    // === Unit tests ===

    #[test]
    fn non_invite_server_new_creates_trying_state() {
        let req = make_request(Method::Register, "z9hG4bKnist_new");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx = NonInviteServerTransaction::new(id.clone(), req.clone(), source);
        assert_eq!(tx.state, NonInviteServerState::Trying);
        assert_eq!(tx.id, id);
        assert!(tx.last_provisional_response.is_none());
        assert!(tx.last_final_response.is_none());
        assert!(tx.timer_j_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
        assert_eq!(tx.source, source);
    }

    #[test]
    fn non_invite_server_on_request_retransmit_in_trying_absorbs() {
        let req = make_request(Method::Register, "z9hG4bKnist_abs");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Trying);
        assert_eq!(tx.retransmit_count, 0);
    }

    #[test]
    fn non_invite_server_on_request_retransmit_in_proceeding_with_provisional() {
        let req = make_request(Method::Register, "z9hG4bKnist_proc");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        // 1xx送信でProceeding状態に遷移
        let resp_100 = make_response(100, "z9hG4bKnist_proc", "REGISTER");
        tx.send_response(100, resp_100.clone(), &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Proceeding);
        // リクエスト再受信 → 暫定レスポンス再送
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::SendProvisionalResponse(resp_100));
        assert_eq!(tx.retransmit_count, 1);
    }

    #[test]
    fn non_invite_server_on_request_retransmit_in_proceeding_without_provisional() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_noprov");
        // 手動でProceeding状態に設定（暫定レスポンスなし）
        tx.state = NonInviteServerState::Proceeding;
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::None);
    }

    #[test]
    fn non_invite_server_on_request_retransmit_in_completed_retransmits_final() {
        let req = make_request(Method::Register, "z9hG4bKnist_comp");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        // 200送信でCompleted状態に遷移
        let resp_200 = make_response(200, "z9hG4bKnist_comp", "REGISTER");
        tx.send_response(200, resp_200.clone(), &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Completed);
        // リクエスト再受信 → 最終レスポンス再送
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::SendFinalResponse(resp_200));
        assert_eq!(tx.retransmit_count, 1);
    }

    #[test]
    fn non_invite_server_on_request_retransmit_in_terminated_ignored() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_term");
        tx.state = NonInviteServerState::Terminated;
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::None);
    }

    #[test]
    fn non_invite_server_send_response_1xx_transitions_to_proceeding() {
        let req = make_request(Method::Register, "z9hG4bKnist_1xx");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let resp = make_response(100, "z9hG4bKnist_1xx", "REGISTER");
        let action = tx.send_response(100, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::SendResponse);
        assert_eq!(tx.state, NonInviteServerState::Proceeding);
        assert!(tx.last_provisional_response.is_some());
        assert!(tx.timer_j_deadline.is_none());
    }

    #[test]
    fn non_invite_server_send_response_2xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_2xx");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKnist_2xx", "REGISTER");
        let action = tx.send_response(200, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::SendResponse);
        assert_eq!(tx.state, NonInviteServerState::Completed);
        assert!(tx.last_final_response.is_some());
        assert!(tx.timer_j_deadline.is_some());
    }

    #[test]
    fn non_invite_server_send_response_4xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_4xx");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let resp = make_response(404, "z9hG4bKnist_4xx", "REGISTER");
        let action = tx.send_response(404, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::SendResponse);
        assert_eq!(tx.state, NonInviteServerState::Completed);
        assert!(tx.last_final_response.is_some());
        assert!(tx.timer_j_deadline.is_some());
    }

    #[test]
    fn non_invite_server_send_response_6xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_6xx");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let resp = make_response(600, "z9hG4bKnist_6xx", "REGISTER");
        let action = tx.send_response(600, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::SendResponse);
        assert_eq!(tx.state, NonInviteServerState::Completed);
    }

    #[test]
    fn non_invite_server_send_response_from_proceeding_2xx_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_p2c");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        // まず1xxでProceedingに
        let resp_100 = make_response(100, "z9hG4bKnist_p2c", "REGISTER");
        tx.send_response(100, resp_100, &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Proceeding);
        // 200でCompletedに
        let resp_200 = make_response(200, "z9hG4bKnist_p2c", "REGISTER");
        let action = tx.send_response(200, resp_200, &timer_config);
        assert_eq!(action, NonInviteServerAction::SendResponse);
        assert_eq!(tx.state, NonInviteServerState::Completed);
        assert!(tx.timer_j_deadline.is_some());
    }

    #[test]
    fn non_invite_server_send_response_not_in_trying_or_proceeding_ignored() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_ign");
        tx.state = NonInviteServerState::Completed;
        let timer_config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKnist_ign", "REGISTER");
        let action = tx.send_response(200, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::None);
    }

    #[test]
    fn non_invite_server_send_response_in_terminated_ignored() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_tign");
        tx.state = NonInviteServerState::Terminated;
        let timer_config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKnist_tign", "REGISTER");
        let action = tx.send_response(200, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::None);
    }

    #[test]
    fn non_invite_server_timer_j_deadline_is_64_times_t1() {
        let req = make_request(Method::Register, "z9hG4bKnist_tj");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let before = Instant::now();
        let resp = make_response(200, "z9hG4bKnist_tj", "REGISTER");
        tx.send_response(200, resp, &timer_config);
        let after = Instant::now();
        let deadline = tx.timer_j_deadline.unwrap();
        // Timer_J = 64 * T1 = 32s
        let expected_duration = timer_config.timer_j();
        assert!(deadline >= before + expected_duration);
        assert!(deadline <= after + expected_duration);
    }

    #[test]
    fn non_invite_server_on_timer_j_terminates_from_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_jterm");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        let resp = make_response(200, "z9hG4bKnist_jterm", "REGISTER");
        tx.send_response(200, resp, &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Completed);
        let action = tx.on_timer_j();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Terminated);
    }

    #[test]
    fn non_invite_server_on_timer_j_no_effect_in_trying() {
        let req = make_request(Method::Register, "z9hG4bKnist_jt");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let action = tx.on_timer_j();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Trying);
    }

    #[test]
    fn non_invite_server_on_timer_j_no_effect_in_proceeding() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_jp");
        tx.state = NonInviteServerState::Proceeding;
        let action = tx.on_timer_j();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Proceeding);
    }

    #[test]
    fn non_invite_server_terminated_ignores_all_events() {
        let mut tx = make_non_invite_server_transaction("z9hG4bKnist_tall");
        tx.state = NonInviteServerState::Terminated;
        let timer_config = TimerConfig::default();

        // on_request_retransmit
        let action = tx.on_request_retransmit();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Terminated);

        // send_response
        let resp = make_response(200, "z9hG4bKnist_tall", "REGISTER");
        let action = tx.send_response(200, resp, &timer_config);
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Terminated);

        // on_timer_j
        let action = tx.on_timer_j();
        assert_eq!(action, NonInviteServerAction::None);
        assert_eq!(tx.state, NonInviteServerState::Terminated);
    }

    #[test]
    fn non_invite_server_full_lifecycle_trying_to_completed_to_terminated() {
        let req = make_request(Method::Register, "z9hG4bKnist_life");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        assert_eq!(tx.state, NonInviteServerState::Trying);

        // 1xx → Proceeding
        let resp_100 = make_response(100, "z9hG4bKnist_life", "REGISTER");
        tx.send_response(100, resp_100, &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Proceeding);

        // 200 → Completed
        let resp_200 = make_response(200, "z9hG4bKnist_life", "REGISTER");
        tx.send_response(200, resp_200, &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Completed);

        // Timer_J → Terminated
        tx.on_timer_j();
        assert_eq!(tx.state, NonInviteServerState::Terminated);
    }

    #[test]
    fn non_invite_server_full_lifecycle_trying_direct_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnist_dir");
        let id = TransactionId::from_request(&req).unwrap();
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let mut tx = NonInviteServerTransaction::new(id, req, source);
        let timer_config = TimerConfig::default();
        assert_eq!(tx.state, NonInviteServerState::Trying);

        // 200 → Completed（Proceedingをスキップ）
        let resp_200 = make_response(200, "z9hG4bKnist_dir", "REGISTER");
        tx.send_response(200, resp_200, &timer_config);
        assert_eq!(tx.state, NonInviteServerState::Completed);

        // Timer_J → Terminated
        tx.on_timer_j();
        assert_eq!(tx.state, NonInviteServerState::Terminated);
    }

    // === Property-based tests ===

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum NonInviteServerEvent {
        RequestRetransmit,
        SendResponse(u16),
        TimerJ,
    }

    /// ランダムなNon-INVITEサーバーイベントを生成するストラテジー
    fn arb_non_invite_server_event() -> impl Strategy<Value = NonInviteServerEvent> {
        prop_oneof![
            Just(NonInviteServerEvent::RequestRetransmit),
            prop_oneof![
                (100u16..200).prop_map(NonInviteServerEvent::SendResponse),
                (200u16..700).prop_map(NonInviteServerEvent::SendResponse),
            ],
            Just(NonInviteServerEvent::TimerJ),
        ]
    }

    /// Non-INVITEサーバートランザクションの状態遷移テーブル
    fn expected_non_invite_server_next_state(
        current: NonInviteServerState,
        event: &NonInviteServerEvent,
    ) -> NonInviteServerState {
        match (current, event) {
            // Trying状態
            (NonInviteServerState::Trying, NonInviteServerEvent::RequestRetransmit) => {
                NonInviteServerState::Trying // 吸収
            }
            (NonInviteServerState::Trying, NonInviteServerEvent::SendResponse(code)) => {
                if (100..200).contains(code) {
                    NonInviteServerState::Proceeding
                } else if (200..700).contains(code) {
                    NonInviteServerState::Completed
                } else {
                    NonInviteServerState::Trying
                }
            }
            (NonInviteServerState::Trying, NonInviteServerEvent::TimerJ) => {
                NonInviteServerState::Trying // Timer_Jは効果なし
            }

            // Proceeding状態
            (NonInviteServerState::Proceeding, NonInviteServerEvent::RequestRetransmit) => {
                NonInviteServerState::Proceeding // 暫定レスポンス再送
            }
            (NonInviteServerState::Proceeding, NonInviteServerEvent::SendResponse(code)) => {
                if (200..700).contains(code) {
                    NonInviteServerState::Completed
                } else {
                    NonInviteServerState::Proceeding
                }
            }
            (NonInviteServerState::Proceeding, NonInviteServerEvent::TimerJ) => {
                NonInviteServerState::Proceeding // Timer_Jは効果なし
            }

            // Completed状態
            (NonInviteServerState::Completed, NonInviteServerEvent::RequestRetransmit) => {
                NonInviteServerState::Completed // 最終レスポンス再送
            }
            (NonInviteServerState::Completed, NonInviteServerEvent::TimerJ) => {
                NonInviteServerState::Terminated
            }
            (NonInviteServerState::Completed, _) => {
                NonInviteServerState::Completed // SendResponseは無視
            }

            // Terminated状態 — すべてのイベントを無視
            (NonInviteServerState::Terminated, _) => NonInviteServerState::Terminated,
        }
    }

    /// Non-INVITEサーバーイベントをトランザクションに適用する
    fn apply_non_invite_server_event(
        tx: &mut NonInviteServerTransaction,
        event: &NonInviteServerEvent,
        timer_config: &TimerConfig,
    ) {
        match event {
            NonInviteServerEvent::RequestRetransmit => {
                tx.on_request_retransmit();
            }
            NonInviteServerEvent::SendResponse(code) => {
                let resp = make_response(*code, &tx.id.branch, "REGISTER");
                tx.send_response(*code, resp, timer_config);
            }
            NonInviteServerEvent::TimerJ => {
                tx.on_timer_j();
            }
        }
    }

    // Feature: sip-transaction-layer, Property 6: Non-INVITEサーバートランザクション状態遷移の正当性
    // **Validates: Requirements 5.1, 5.3, 5.5, 5.7**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn non_invite_server_state_transitions_match_table(
            events in proptest::collection::vec(arb_non_invite_server_event(), 1..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Register, "z9hG4bKproptest_nis");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = NonInviteServerTransaction::new(id, req, source);

            for event in &events {
                let expected = expected_non_invite_server_next_state(tx.state, event);
                apply_non_invite_server_event(&mut tx, event, &timer_config);
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

    // Feature: sip-transaction-layer, Property 14: リクエスト再受信の吸収（サーバートランザクション）
    // **Validates: Requirements 5.2, 9.4**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn non_invite_server_trying_absorbs_request_retransmit(
            n in 1usize..=50,
        ) {
            let req = make_request(Method::Register, "z9hG4bKproptest_abs");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = NonInviteServerTransaction::new(id, req, source);

            // Trying状態でn回リクエスト再受信を適用
            for _ in 0..n {
                let action = tx.on_request_retransmit();
                // 上位層に通知されない（Noneアクション）
                prop_assert_eq!(
                    action,
                    NonInviteServerAction::None,
                    "Request retransmit in Trying state should be absorbed (None action)",
                );
                // 状態が変化しない
                prop_assert_eq!(
                    tx.state,
                    NonInviteServerState::Trying,
                    "State should remain Trying after request retransmit absorption",
                );
                // retransmit_countが増加しない
                prop_assert_eq!(
                    tx.retransmit_count,
                    0,
                    "Retransmit count should not increase when absorbing in Trying state",
                );
            }
        }
    }
}
