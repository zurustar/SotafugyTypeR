use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::sip::message::{SipRequest, SipResponse};

use super::{TimerConfig, TransactionId};

/// クライアントトランザクション（Non-INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInviteClientState {
    Trying,
    Proceeding,
    Completed,
    Terminated,
}

/// Non-INVITEクライアントトランザクションのアクション
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInviteClientAction {
    /// アクション不要（既にTerminated、またはイベント無視）
    None,
    /// リクエスト再送
    Retransmit,
    /// レスポンス受信、上位層に渡す
    ResponseReceived,
    /// トランザクション終了（タイムアウト）
    Terminated,
}

/// Non-INVITEクライアントトランザクション
pub struct NonInviteClientTransaction {
    pub id: TransactionId,
    pub state: NonInviteClientState,
    pub original_request: SipRequest,
    pub last_response: Option<SipResponse>,
    pub destination: SocketAddr,
    /// Timer_E: 再送タイマー（次回発火時刻）
    pub timer_e_next: Option<Instant>,
    /// Timer_E: 現在の再送間隔
    pub timer_e_interval: Duration,
    /// Timer_F: タイムアウトタイマー期限
    pub timer_f_deadline: Instant,
    /// Timer_K: Completed状態待機タイマー期限
    pub timer_k_deadline: Option<Instant>,
    pub retransmit_count: u32,
}

impl NonInviteClientTransaction {
    /// Trying状態で新しいNon-INVITEクライアントトランザクションを作成
    /// Timer_E（初期値T1）とTimer_F（64*T1）を開始
    pub fn new(
        id: TransactionId,
        request: SipRequest,
        destination: SocketAddr,
        timer_config: &TimerConfig,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            state: NonInviteClientState::Trying,
            original_request: request,
            last_response: None,
            destination,
            timer_e_next: Some(now + timer_config.t1),
            timer_e_interval: timer_config.t1,
            timer_f_deadline: now + timer_config.timer_f(),
            timer_k_deadline: None,
            retransmit_count: 0,
        }
    }

    /// レスポンス受信時の状態遷移
    /// ステータスコードに応じた状態遷移を行い、呼び出し元が取るべきアクションを返す
    pub fn on_response(&mut self, status_code: u16, timer_config: &TimerConfig) -> NonInviteClientAction {
        match self.state {
            NonInviteClientState::Trying => {
                if (100..200).contains(&status_code) {
                    // 1xx → Proceeding
                    self.state = NonInviteClientState::Proceeding;
                    NonInviteClientAction::ResponseReceived
                } else if (200..700).contains(&status_code) {
                    // 2xx-6xx → Completed, Timer_K開始
                    self.state = NonInviteClientState::Completed;
                    self.timer_e_next = None;
                    self.timer_k_deadline = Some(Instant::now() + timer_config.timer_k());
                    NonInviteClientAction::ResponseReceived
                } else {
                    NonInviteClientAction::None
                }
            }
            NonInviteClientState::Proceeding => {
                if (200..700).contains(&status_code) {
                    // 2xx-6xx → Completed, Timer_K開始
                    self.state = NonInviteClientState::Completed;
                    self.timer_e_next = None;
                    self.timer_k_deadline = Some(Instant::now() + timer_config.timer_k());
                    NonInviteClientAction::ResponseReceived
                } else {
                    NonInviteClientAction::None
                }
            }
            NonInviteClientState::Completed | NonInviteClientState::Terminated => {
                NonInviteClientAction::None
            }
        }
    }

    /// Timer_E発火時の処理（リクエスト再送）
    /// Trying状態: Timer_E間隔をmin(2*interval, T2)に更新
    /// Proceeding状態: Timer_E間隔をT2に設定
    /// 再送すべき場合はtrueを返す
    pub fn on_timer_e(&mut self, timer_config: &TimerConfig) -> bool {
        match self.state {
            NonInviteClientState::Trying => {
                self.timer_e_interval = (self.timer_e_interval * 2).min(timer_config.t2);
                self.timer_e_next = Some(Instant::now() + self.timer_e_interval);
                self.retransmit_count += 1;
                true
            }
            NonInviteClientState::Proceeding => {
                self.timer_e_interval = timer_config.t2;
                self.timer_e_next = Some(Instant::now() + self.timer_e_interval);
                self.retransmit_count += 1;
                true
            }
            _ => false,
        }
    }

    /// Timer_F発火時の処理（タイムアウト）
    /// Trying/Proceeding状態の場合にTerminated状態に遷移する
    /// タイムアウトが発生した場合はtrueを返す
    pub fn on_timer_f(&mut self) -> bool {
        match self.state {
            NonInviteClientState::Trying | NonInviteClientState::Proceeding => {
                self.state = NonInviteClientState::Terminated;
                true
            }
            _ => false,
        }
    }

    /// Timer_K発火時の処理（Completed状態待機終了）
    /// Completed状態の場合にTerminated状態に遷移する
    /// 遷移が発生した場合はtrueを返す
    pub fn on_timer_k(&mut self) -> bool {
        if self.state != NonInviteClientState::Completed {
            return false;
        }
        self.state = NonInviteClientState::Terminated;
        true
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

    fn make_non_invite_client_transaction(branch: &str) -> NonInviteClientTransaction {
        let req = make_request(Method::Register, branch);
        let id = TransactionId::from_request(&req).unwrap();
        let now = Instant::now();
        let config = TimerConfig::default();
        NonInviteClientTransaction {
            id,
            state: NonInviteClientState::Trying,
            original_request: req,
            last_response: None,
            destination: "127.0.0.1:5060".parse().unwrap(),
            timer_e_next: Some(now + config.t1),
            timer_e_interval: config.t1,
            timer_f_deadline: now + config.timer_f(),
            timer_k_deadline: None,
            retransmit_count: 0,
        }
    }

    #[test]
    fn non_invite_client_state_variants() {
        let states = [
            NonInviteClientState::Trying,
            NonInviteClientState::Proceeding,
            NonInviteClientState::Completed,
            NonInviteClientState::Terminated,
        ];
        // All variants are distinct
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j]);
            }
        }
    }

    #[test]
    fn non_invite_client_transaction_initial_state() {
        let tx = make_non_invite_client_transaction("z9hG4bKnict001");
        assert_eq!(tx.state, NonInviteClientState::Trying);
        assert_eq!(tx.id.branch, "z9hG4bKnict001");
        assert_eq!(tx.id.method, Method::Register);
        assert!(tx.last_response.is_none());
        assert!(tx.timer_e_next.is_some());
        assert!(tx.timer_k_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
    }

    #[test]
    fn non_invite_client_new_creates_trying_state() {
        let req = make_request(Method::Register, "z9hG4bKnew001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let before = Instant::now();
        let tx = NonInviteClientTransaction::new(id, req, dest, &config);
        let after = Instant::now();

        assert_eq!(tx.state, NonInviteClientState::Trying);
        assert!(tx.timer_e_next.is_some());
        assert_eq!(tx.timer_e_interval, config.t1);
        assert!(tx.timer_k_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
        // Timer_F deadline should be ~64*T1 from now
        let expected_f = config.timer_f();
        assert!(tx.timer_f_deadline >= before + expected_f);
        assert!(tx.timer_f_deadline <= after + expected_f);
    }

    #[test]
    fn non_invite_client_trying_1xx_transitions_to_proceeding() {
        let req = make_request(Method::Register, "z9hG4bKni1xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(100, &config);
        assert_eq!(tx.state, NonInviteClientState::Proceeding);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
    }

    #[test]
    fn non_invite_client_trying_2xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKni2xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(200, &config);
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
        assert!(tx.timer_k_deadline.is_some());
    }

    #[test]
    fn non_invite_client_trying_4xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKni4xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(404, &config);
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
        assert!(tx.timer_k_deadline.is_some());
    }

    #[test]
    fn non_invite_client_trying_6xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKni6xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(603, &config);
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
    }

    #[test]
    fn non_invite_client_proceeding_2xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnip2xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let action = tx.on_response(200, &config);
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
        assert!(tx.timer_k_deadline.is_some());
    }

    #[test]
    fn non_invite_client_proceeding_4xx_transitions_to_completed() {
        let req = make_request(Method::Register, "z9hG4bKnip4xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let action = tx.on_response(404, &config);
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::ResponseReceived);
    }

    #[test]
    fn non_invite_client_on_timer_e_retransmits_in_trying() {
        let req = make_request(Method::Register, "z9hG4bKnite001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let result = tx.on_timer_e(&config);
        assert!(result);
        assert_eq!(tx.retransmit_count, 1);
        // Trying: interval = min(2*T1, T2) = min(1000ms, 4000ms) = 1000ms
        assert_eq!(tx.timer_e_interval, Duration::from_millis(1000));
    }

    #[test]
    fn non_invite_client_on_timer_e_doubles_interval_capped_by_t2() {
        let req = make_request(Method::Register, "z9hG4bKnite002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        // T1=500ms, T2=4s
        // After 1st: min(1000ms, 4s) = 1000ms
        tx.on_timer_e(&config);
        assert_eq!(tx.timer_e_interval, Duration::from_millis(1000));
        // After 2nd: min(2000ms, 4s) = 2000ms
        tx.on_timer_e(&config);
        assert_eq!(tx.timer_e_interval, Duration::from_millis(2000));
        // After 3rd: min(4000ms, 4s) = 4000ms
        tx.on_timer_e(&config);
        assert_eq!(tx.timer_e_interval, Duration::from_millis(4000));
        // After 4th: min(8000ms, 4s) = 4000ms (capped at T2)
        tx.on_timer_e(&config);
        assert_eq!(tx.timer_e_interval, Duration::from_secs(4));
    }

    #[test]
    fn non_invite_client_on_timer_e_in_proceeding_uses_t2() {
        let req = make_request(Method::Register, "z9hG4bKnite003");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let result = tx.on_timer_e(&config);
        assert!(result);
        // Proceeding: interval is always T2
        assert_eq!(tx.timer_e_interval, config.t2);
    }

    #[test]
    fn non_invite_client_on_timer_e_no_retransmit_in_completed() {
        let req = make_request(Method::Register, "z9hG4bKnite004");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(200, &config); // → Completed
        let result = tx.on_timer_e(&config);
        assert!(!result);
    }

    #[test]
    fn non_invite_client_on_timer_f_terminates_from_trying() {
        let req = make_request(Method::Register, "z9hG4bKnitf001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let result = tx.on_timer_f();
        assert!(result);
        assert_eq!(tx.state, NonInviteClientState::Terminated);
    }

    #[test]
    fn non_invite_client_on_timer_f_terminates_from_proceeding() {
        let req = make_request(Method::Register, "z9hG4bKnitf002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let result = tx.on_timer_f();
        assert!(result);
        assert_eq!(tx.state, NonInviteClientState::Terminated);
    }

    #[test]
    fn non_invite_client_on_timer_f_no_effect_in_completed() {
        let req = make_request(Method::Register, "z9hG4bKnitf003");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(200, &config); // → Completed
        let result = tx.on_timer_f();
        assert!(!result);
        assert_eq!(tx.state, NonInviteClientState::Completed);
    }

    #[test]
    fn non_invite_client_on_timer_k_terminates_from_completed() {
        let req = make_request(Method::Register, "z9hG4bKnitk001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(200, &config); // → Completed
        let result = tx.on_timer_k();
        assert!(result);
        assert_eq!(tx.state, NonInviteClientState::Terminated);
    }

    #[test]
    fn non_invite_client_on_timer_k_no_effect_in_trying() {
        let req = make_request(Method::Register, "z9hG4bKnitk002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        let result = tx.on_timer_k();
        assert!(!result);
        assert_eq!(tx.state, NonInviteClientState::Trying);
    }

    #[test]
    fn non_invite_client_terminated_ignores_all_events() {
        let req = make_request(Method::Register, "z9hG4bKniterm001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_timer_f(); // → Terminated

        // All events should be ignored
        let action = tx.on_response(200, &config);
        assert_eq!(tx.state, NonInviteClientState::Terminated);
        assert_eq!(action, NonInviteClientAction::None);

        assert!(!tx.on_timer_e(&config));
        assert!(!tx.on_timer_f());
        assert!(!tx.on_timer_k());
    }

    #[test]
    fn non_invite_client_completed_1xx_ignored() {
        let req = make_request(Method::Register, "z9hG4bKnic1xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = NonInviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(200, &config); // → Completed
        let action = tx.on_response(100, &config); // 1xx in Completed
        assert_eq!(tx.state, NonInviteClientState::Completed);
        assert_eq!(action, NonInviteClientAction::None);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Non-INVITEクライアントトランザクションに適用するイベント
        #[derive(Debug, Clone)]
        enum NonInviteClientEvent {
            Response(u16),
            TimerE,
            TimerF,
            TimerK,
        }

        /// ランダムなNon-INVITEクライアントイベントを生成するストラテジー
        fn arb_non_invite_client_event() -> impl Strategy<Value = NonInviteClientEvent> {
            prop_oneof![
                (100u16..700).prop_map(NonInviteClientEvent::Response),
                Just(NonInviteClientEvent::TimerE),
                Just(NonInviteClientEvent::TimerF),
                Just(NonInviteClientEvent::TimerK),
            ]
        }

        /// Non-INVITEクライアントトランザクションの状態遷移テーブル
        fn expected_non_invite_next_state(
            current: NonInviteClientState,
            event: &NonInviteClientEvent,
        ) -> NonInviteClientState {
            match (current, event) {
                // Trying状態
                (NonInviteClientState::Trying, NonInviteClientEvent::Response(code)) => {
                    if (100..200).contains(code) {
                        NonInviteClientState::Proceeding
                    } else if (200..700).contains(code) {
                        NonInviteClientState::Completed
                    } else {
                        NonInviteClientState::Trying
                    }
                }
                (NonInviteClientState::Trying, NonInviteClientEvent::TimerE) => {
                    NonInviteClientState::Trying
                }
                (NonInviteClientState::Trying, NonInviteClientEvent::TimerF) => {
                    NonInviteClientState::Terminated
                }
                (NonInviteClientState::Trying, NonInviteClientEvent::TimerK) => {
                    NonInviteClientState::Trying // Timer_K has no effect in Trying
                }

                // Proceeding状態
                (NonInviteClientState::Proceeding, NonInviteClientEvent::Response(code)) => {
                    if (200..700).contains(code) {
                        NonInviteClientState::Completed
                    } else {
                        NonInviteClientState::Proceeding // 1xx and others ignored
                    }
                }
                (NonInviteClientState::Proceeding, NonInviteClientEvent::TimerE) => {
                    NonInviteClientState::Proceeding
                }
                (NonInviteClientState::Proceeding, NonInviteClientEvent::TimerF) => {
                    NonInviteClientState::Terminated
                }
                (NonInviteClientState::Proceeding, NonInviteClientEvent::TimerK) => {
                    NonInviteClientState::Proceeding // Timer_K has no effect in Proceeding
                }

                // Completed状態
                (NonInviteClientState::Completed, NonInviteClientEvent::TimerK) => {
                    NonInviteClientState::Terminated
                }
                (NonInviteClientState::Completed, _) => {
                    NonInviteClientState::Completed // All other events ignored
                }

                // Terminated状態 — すべてのイベントを無視
                (NonInviteClientState::Terminated, _) => NonInviteClientState::Terminated,
            }
        }

        /// Non-INVITEクライアントイベントをトランザクションに適用する
        fn apply_non_invite_event(
            tx: &mut NonInviteClientTransaction,
            event: &NonInviteClientEvent,
            timer_config: &TimerConfig,
        ) {
            match event {
                NonInviteClientEvent::Response(code) => {
                    tx.on_response(*code, timer_config);
                }
                NonInviteClientEvent::TimerE => {
                    tx.on_timer_e(timer_config);
                }
                NonInviteClientEvent::TimerF => {
                    tx.on_timer_f();
                }
                NonInviteClientEvent::TimerK => {
                    tx.on_timer_k();
                }
            }
        }

        // Feature: sip-transaction-layer, Property 4: Non-INVITEクライアントトランザクション状態遷移の正当性
        // **Validates: Requirements 3.1, 3.3, 3.5, 3.6, 3.7**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn non_invite_client_state_transitions_match_table(
                events in proptest::collection::vec(arb_non_invite_client_event(), 1..20),
            ) {
                let timer_config = TimerConfig::default();
                let req = make_request(Method::Register, "z9hG4bKproptest_ni");
                let id = TransactionId::from_request(&req).unwrap();
                let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
                let mut tx = NonInviteClientTransaction::new(id, req, dest, &timer_config);

                for event in &events {
                    let expected = expected_non_invite_next_state(tx.state, event);
                    apply_non_invite_event(&mut tx, event, &timer_config);
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

        // Feature: sip-transaction-layer, Property 11: Timer_E再送間隔の上限
        // **Validates: Requirements 3.2, 3.4, 6.5, 13.6**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn timer_e_interval_never_exceeds_t2(
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
                let req = make_request(Method::Register, "z9hG4bKtimerE");
                let id = TransactionId::from_request(&req).unwrap();
                let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
                let mut tx = NonInviteClientTransaction::new(id, req, dest, &timer_config);

                // 初期状態: timer_e_interval == T1
                prop_assert_eq!(tx.timer_e_interval, t1);

                // n回 on_timer_e() を呼び出し、毎回 interval <= T2 を検証
                for i in 0..n {
                    tx.on_timer_e(&timer_config);
                    prop_assert!(
                        tx.timer_e_interval <= t2,
                        "After {} retransmissions with T1={:?}, T2={:?}: interval {:?} exceeds T2",
                        i + 1,
                        t1,
                        t2,
                        tx.timer_e_interval,
                    );
                }
            }
        }
    }
}
