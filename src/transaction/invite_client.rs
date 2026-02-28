use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::sip::message::{SipRequest, SipResponse};

use super::{TimerConfig, TransactionId};

/// クライアントトランザクション（INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteClientState {
    Calling,
    Proceeding,
    Completed,
    Terminated,
}

/// INVITEクライアントトランザクションの状態遷移後に呼び出し元が取るべきアクション
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteClientAction {
    /// アクション不要（既にTerminated、またはイベント無視）
    None,
    /// エラーレスポンスに対するACK送信、Completed状態に遷移
    SendAck,
    /// レスポンス受信、上位層に渡す
    ResponseReceived,
    /// トランザクション終了（2xx受信またはタイムアウト）
    Terminated,
}

/// INVITEクライアントトランザクション
pub struct InviteClientTransaction {
    pub id: TransactionId,
    pub state: InviteClientState,
    pub original_request: SipRequest,
    pub last_response: Option<SipResponse>,
    pub destination: SocketAddr,
    /// Timer_A: 再送タイマー（次回発火時刻）
    pub timer_a_next: Option<Instant>,
    /// Timer_A: 現在の再送間隔
    pub timer_a_interval: Duration,
    /// Timer_B: タイムアウトタイマー期限
    pub timer_b_deadline: Instant,
    /// Timer_D: Completed状態待機タイマー期限
    pub timer_d_deadline: Option<Instant>,
    pub retransmit_count: u32,
}

impl InviteClientTransaction {
    /// Calling状態で新しいINVITEクライアントトランザクションを作成
    /// Timer_A（初期値T1）とTimer_B（64*T1）を開始
    pub fn new(
        id: TransactionId,
        request: SipRequest,
        destination: SocketAddr,
        timer_config: &TimerConfig,
    ) -> Self {
        let now = Instant::now();
        Self {
            id,
            state: InviteClientState::Calling,
            original_request: request,
            last_response: None,
            destination,
            timer_a_next: Some(now + timer_config.t1),
            timer_a_interval: timer_config.t1,
            timer_b_deadline: now + timer_config.timer_b(),
            timer_d_deadline: None,
            retransmit_count: 0,
        }
    }

    /// レスポンス受信時の状態遷移
    /// ステータスコードに応じた状態遷移を行い、呼び出し元が取るべきアクションを返す
    pub fn on_response(&mut self, status_code: u16, timer_config: &TimerConfig) -> InviteClientAction {
        match self.state {
            InviteClientState::Calling => {
                if (100..200).contains(&status_code) {
                    // 1xx → Proceeding, Timer_A停止
                    self.state = InviteClientState::Proceeding;
                    self.timer_a_next = None;
                    InviteClientAction::ResponseReceived
                } else if (200..300).contains(&status_code) {
                    // 2xx → Terminated
                    self.state = InviteClientState::Terminated;
                    InviteClientAction::Terminated
                } else if (300..700).contains(&status_code) {
                    // 3xx-6xx → Completed, ACK送信, Timer_D開始
                    self.state = InviteClientState::Completed;
                    self.timer_a_next = None;
                    self.timer_d_deadline = Some(Instant::now() + timer_config.timer_d());
                    InviteClientAction::SendAck
                } else {
                    InviteClientAction::None
                }
            }
            InviteClientState::Proceeding => {
                if (100..200).contains(&status_code) {
                    // 追加の1xx → Proceeding維持、上位層に通知
                    InviteClientAction::ResponseReceived
                } else if (200..300).contains(&status_code) {
                    // 2xx → Terminated
                    self.state = InviteClientState::Terminated;
                    InviteClientAction::Terminated
                } else if (300..700).contains(&status_code) {
                    // 3xx-6xx → Completed, ACK送信, Timer_D開始
                    self.state = InviteClientState::Completed;
                    self.timer_d_deadline = Some(Instant::now() + timer_config.timer_d());
                    InviteClientAction::SendAck
                } else {
                    InviteClientAction::None
                }
            }
            InviteClientState::Completed => {
                if (300..700).contains(&status_code) {
                    // 3xx-6xx再受信 → ACK再送
                    InviteClientAction::SendAck
                } else {
                    InviteClientAction::None
                }
            }
            InviteClientState::Terminated => {
                // Terminated状態ではすべてのイベントを無視
                InviteClientAction::None
            }
        }
    }

    /// Timer_A発火時の処理（INVITE再送）
    /// Calling状態の場合のみ再送を行い、Timer_A間隔を2倍に更新する
    /// 再送すべき場合はtrueを返す
    pub fn on_timer_a(&mut self) -> bool {
        if self.state != InviteClientState::Calling {
            return false;
        }
        // Timer_A間隔を2倍に更新
        self.timer_a_interval *= 2;
        self.timer_a_next = Some(Instant::now() + self.timer_a_interval);
        self.retransmit_count += 1;
        true
    }

    /// Timer_B発火時の処理（タイムアウト）
    /// Calling状態の場合にTerminated状態に遷移する
    /// タイムアウトが発生した場合はtrueを返す
    pub fn on_timer_b(&mut self) -> bool {
        match self.state {
            InviteClientState::Calling => {
                self.state = InviteClientState::Terminated;
                true
            }
            _ => false,
        }
    }

    /// Timer_D発火時の処理（Completed状態待機終了）
    /// Completed状態の場合にTerminated状態に遷移する
    /// 遷移が発生した場合はtrueを返す
    pub fn on_timer_d(&mut self) -> bool {
        if self.state != InviteClientState::Completed {
            return false;
        }
        self.state = InviteClientState::Terminated;
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

    fn make_invite_client_transaction(branch: &str) -> InviteClientTransaction {
        let req = make_request(Method::Invite, branch);
        let id = TransactionId::from_request(&req).unwrap();
        let now = Instant::now();
        let config = TimerConfig::default();
        InviteClientTransaction {
            id,
            state: InviteClientState::Calling,
            original_request: req,
            last_response: None,
            destination: "127.0.0.1:5060".parse().unwrap(),
            timer_a_next: Some(now + config.t1),
            timer_a_interval: config.t1,
            timer_b_deadline: now + config.timer_b(),
            timer_d_deadline: None,
            retransmit_count: 0,
        }
    }

    #[test]
    fn invite_client_state_variants() {
        let states = [
            InviteClientState::Calling,
            InviteClientState::Proceeding,
            InviteClientState::Completed,
            InviteClientState::Terminated,
        ];
        // All variants are distinct
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j]);
            }
        }
    }

    #[test]
    fn invite_client_transaction_initial_state() {
        let tx = make_invite_client_transaction("z9hG4bKict001");
        assert_eq!(tx.state, InviteClientState::Calling);
        assert_eq!(tx.id.branch, "z9hG4bKict001");
        assert_eq!(tx.id.method, Method::Invite);
        assert!(tx.last_response.is_none());
        assert!(tx.timer_a_next.is_some());
        assert!(tx.timer_d_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
    }

    #[test]
    fn invite_client_new_creates_calling_state() {
        let req = make_request(Method::Invite, "z9hG4bKnew001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();

        let tx = InviteClientTransaction::new(id.clone(), req, dest, &config);

        assert_eq!(tx.state, InviteClientState::Calling);
        assert_eq!(tx.id, id);
        assert_eq!(tx.destination, dest);
        assert!(tx.timer_a_next.is_some());
        assert_eq!(tx.timer_a_interval, config.t1);
        assert!(tx.timer_d_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
        assert!(tx.last_response.is_none());
    }

    #[test]
    fn invite_client_calling_1xx_transitions_to_proceeding() {
        let req = make_request(Method::Invite, "z9hG4bK1xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(100, &config);

        assert_eq!(tx.state, InviteClientState::Proceeding);
        assert_eq!(action, InviteClientAction::ResponseReceived);
        // Timer_A should be stopped
        assert!(tx.timer_a_next.is_none());
    }

    #[test]
    fn invite_client_calling_2xx_transitions_to_terminated() {
        let req = make_request(Method::Invite, "z9hG4bK2xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(200, &config);

        assert_eq!(tx.state, InviteClientState::Terminated);
        assert_eq!(action, InviteClientAction::Terminated);
    }

    #[test]
    fn invite_client_calling_3xx_transitions_to_completed() {
        let req = make_request(Method::Invite, "z9hG4bK3xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(300, &config);

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::SendAck);
        assert!(tx.timer_d_deadline.is_some());
    }

    #[test]
    fn invite_client_calling_4xx_transitions_to_completed() {
        let req = make_request(Method::Invite, "z9hG4bK4xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(404, &config);

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::SendAck);
        assert!(tx.timer_d_deadline.is_some());
    }

    #[test]
    fn invite_client_calling_6xx_transitions_to_completed() {
        let req = make_request(Method::Invite, "z9hG4bK6xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let action = tx.on_response(600, &config);

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::SendAck);
    }

    #[test]
    fn invite_client_proceeding_1xx_stays_proceeding() {
        let req = make_request(Method::Invite, "z9hG4bKp1xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let action = tx.on_response(180, &config); // 追加の1xx

        assert_eq!(tx.state, InviteClientState::Proceeding);
        assert_eq!(action, InviteClientAction::ResponseReceived);
    }

    #[test]
    fn invite_client_proceeding_2xx_transitions_to_terminated() {
        let req = make_request(Method::Invite, "z9hG4bKp2xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let action = tx.on_response(200, &config);

        assert_eq!(tx.state, InviteClientState::Terminated);
        assert_eq!(action, InviteClientAction::Terminated);
    }

    #[test]
    fn invite_client_proceeding_3xx_6xx_transitions_to_completed() {
        let req = make_request(Method::Invite, "z9hG4bKp3xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let action = tx.on_response(486, &config);

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::SendAck);
        assert!(tx.timer_d_deadline.is_some());
    }

    #[test]
    fn invite_client_completed_3xx_6xx_retransmits_ack() {
        let req = make_request(Method::Invite, "z9hG4bKcack001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(486, &config); // → Completed
        let action = tx.on_response(486, &config); // 再受信

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::SendAck);
    }

    #[test]
    fn invite_client_completed_1xx_ignored() {
        let req = make_request(Method::Invite, "z9hG4bKcign001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(486, &config); // → Completed
        let action = tx.on_response(100, &config); // 1xx in Completed

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::None);
    }

    #[test]
    fn invite_client_terminated_ignores_all_events() {
        let req = make_request(Method::Invite, "z9hG4bKtign001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(200, &config); // → Terminated
        assert_eq!(tx.state, InviteClientState::Terminated);

        let action = tx.on_response(200, &config);
        assert_eq!(tx.state, InviteClientState::Terminated);
        assert_eq!(action, InviteClientAction::None);

        assert!(!tx.on_timer_a());
        assert!(!tx.on_timer_b());
        assert!(!tx.on_timer_d());
    }

    #[test]
    fn invite_client_on_timer_a_retransmits_in_calling() {
        let req = make_request(Method::Invite, "z9hG4bKta001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let initial_interval = tx.timer_a_interval;
        let result = tx.on_timer_a();

        assert!(result);
        assert_eq!(tx.state, InviteClientState::Calling);
        assert_eq!(tx.timer_a_interval, initial_interval * 2);
        assert_eq!(tx.retransmit_count, 1);
    }

    #[test]
    fn invite_client_on_timer_a_doubles_interval_each_time() {
        let req = make_request(Method::Invite, "z9hG4bKta002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let t1 = config.t1;
        tx.on_timer_a(); // interval: T1 → 2*T1
        assert_eq!(tx.timer_a_interval, t1 * 2);
        tx.on_timer_a(); // interval: 2*T1 → 4*T1
        assert_eq!(tx.timer_a_interval, t1 * 4);
        tx.on_timer_a(); // interval: 4*T1 → 8*T1
        assert_eq!(tx.timer_a_interval, t1 * 8);
        assert_eq!(tx.retransmit_count, 3);
    }

    #[test]
    fn invite_client_on_timer_a_no_retransmit_in_proceeding() {
        let req = make_request(Method::Invite, "z9hG4bKta003");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(100, &config); // → Proceeding
        let result = tx.on_timer_a();

        assert!(!result);
        assert_eq!(tx.state, InviteClientState::Proceeding);
    }

    #[test]
    fn invite_client_on_timer_b_terminates_from_calling() {
        let req = make_request(Method::Invite, "z9hG4bKtb001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let result = tx.on_timer_b();

        assert!(result);
        assert_eq!(tx.state, InviteClientState::Terminated);
    }

    #[test]
    fn invite_client_on_timer_b_no_effect_in_completed() {
        let req = make_request(Method::Invite, "z9hG4bKtb002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(486, &config); // → Completed
        let result = tx.on_timer_b();

        assert!(!result);
        assert_eq!(tx.state, InviteClientState::Completed);
    }

    #[test]
    fn invite_client_on_timer_d_terminates_from_completed() {
        let req = make_request(Method::Invite, "z9hG4bKtd001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(486, &config); // → Completed
        let result = tx.on_timer_d();

        assert!(result);
        assert_eq!(tx.state, InviteClientState::Terminated);
    }

    #[test]
    fn invite_client_on_timer_d_no_effect_in_calling() {
        let req = make_request(Method::Invite, "z9hG4bKtd002");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        let result = tx.on_timer_d();

        assert!(!result);
        assert_eq!(tx.state, InviteClientState::Calling);
    }

    #[test]
    fn invite_client_new_timer_b_deadline_is_64_times_t1() {
        let req = make_request(Method::Invite, "z9hG4bKtbd001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let before = Instant::now();
        let tx = InviteClientTransaction::new(id, req, dest, &config);
        let after = Instant::now();

        // timer_b_deadline should be approximately now + 64*T1
        let expected_duration = config.timer_b();
        assert!(tx.timer_b_deadline >= before + expected_duration);
        assert!(tx.timer_b_deadline <= after + expected_duration);
    }

    #[test]
    fn invite_client_completed_2xx_ignored() {
        let req = make_request(Method::Invite, "z9hG4bKc2xx001");
        let id = TransactionId::from_request(&req).unwrap();
        let config = TimerConfig::default();
        let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let mut tx = InviteClientTransaction::new(id, req, dest, &config);

        tx.on_response(486, &config); // → Completed
        let action = tx.on_response(200, &config); // 2xx in Completed

        assert_eq!(tx.state, InviteClientState::Completed);
        assert_eq!(action, InviteClientAction::None);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// ランダムなINVITEクライアントイベントを生成するストラテジー
        #[derive(Debug, Clone)]
        enum InviteClientEvent {
            Response(u16),
            TimerA,
            TimerB,
            TimerD,
        }

        fn arb_invite_client_event() -> impl Strategy<Value = InviteClientEvent> {
            prop_oneof![
                (100u16..700).prop_map(InviteClientEvent::Response),
                Just(InviteClientEvent::TimerA),
                Just(InviteClientEvent::TimerB),
                Just(InviteClientEvent::TimerD),
            ]
        }

        /// 現在の状態とイベントから期待される次の状態を計算する（状態遷移テーブル）
        fn expected_next_state(
            current: InviteClientState,
            event: &InviteClientEvent,
        ) -> InviteClientState {
            match (current, event) {
                // Calling状態
                (InviteClientState::Calling, InviteClientEvent::Response(code)) => {
                    if (100..200).contains(code) {
                        InviteClientState::Proceeding
                    } else if (200..300).contains(code) {
                        InviteClientState::Terminated
                    } else if (300..700).contains(code) {
                        InviteClientState::Completed
                    } else {
                        InviteClientState::Calling
                    }
                }
                (InviteClientState::Calling, InviteClientEvent::TimerA) => {
                    InviteClientState::Calling
                }
                (InviteClientState::Calling, InviteClientEvent::TimerB) => {
                    InviteClientState::Terminated
                }
                (InviteClientState::Calling, InviteClientEvent::TimerD) => {
                    InviteClientState::Calling // Timer_D has no effect in Calling
                }

                // Proceeding状態
                (InviteClientState::Proceeding, InviteClientEvent::Response(code)) => {
                    if (100..200).contains(code) {
                        InviteClientState::Proceeding
                    } else if (200..300).contains(code) {
                        InviteClientState::Terminated
                    } else if (300..700).contains(code) {
                        InviteClientState::Completed
                    } else {
                        InviteClientState::Proceeding
                    }
                }
                (InviteClientState::Proceeding, InviteClientEvent::TimerA) => {
                    InviteClientState::Proceeding // Timer_A has no effect in Proceeding
                }
                (InviteClientState::Proceeding, InviteClientEvent::TimerB) => {
                    InviteClientState::Proceeding // Timer_B has no effect in Proceeding
                }
                (InviteClientState::Proceeding, InviteClientEvent::TimerD) => {
                    InviteClientState::Proceeding // Timer_D has no effect in Proceeding
                }

                // Completed状態
                (InviteClientState::Completed, InviteClientEvent::Response(code)) => {
                    if (300..700).contains(code) {
                        InviteClientState::Completed // ACK retransmit, stay Completed
                    } else {
                        InviteClientState::Completed // Other responses ignored
                    }
                }
                (InviteClientState::Completed, InviteClientEvent::TimerA) => {
                    InviteClientState::Completed // Timer_A has no effect in Completed
                }
                (InviteClientState::Completed, InviteClientEvent::TimerB) => {
                    InviteClientState::Completed // Timer_B has no effect in Completed
                }
                (InviteClientState::Completed, InviteClientEvent::TimerD) => {
                    InviteClientState::Terminated
                }

                // Terminated状態 — すべてのイベントを無視
                (InviteClientState::Terminated, _) => InviteClientState::Terminated,
            }
        }

        /// イベントをトランザクションに適用する
        fn apply_event(
            tx: &mut InviteClientTransaction,
            event: &InviteClientEvent,
            timer_config: &TimerConfig,
        ) {
            match event {
                InviteClientEvent::Response(code) => {
                    tx.on_response(*code, timer_config);
                }
                InviteClientEvent::TimerA => {
                    tx.on_timer_a();
                }
                InviteClientEvent::TimerB => {
                    tx.on_timer_b();
                }
                InviteClientEvent::TimerD => {
                    tx.on_timer_d();
                }
            }
        }

        // Feature: sip-transaction-layer, Property 3: INVITEクライアントトランザクション状態遷移の正当性
        // **Validates: Requirements 2.1, 2.3, 2.4, 2.5, 2.6, 2.7, 2.8, 2.9**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn invite_client_state_transitions_match_table(
                events in proptest::collection::vec(arb_invite_client_event(), 1..20),
            ) {
                let timer_config = TimerConfig::default();
                let req = make_request(Method::Invite, "z9hG4bKproptest");
                let id = TransactionId::from_request(&req).unwrap();
                let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
                let mut tx = InviteClientTransaction::new(id, req, dest, &timer_config);

                for event in &events {
                    let expected = expected_next_state(tx.state, event);
                    apply_event(&mut tx, event, &timer_config);
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

        // Feature: sip-transaction-layer, Property 10: Timer_A再送間隔の倍増
        // **Validates: Requirements 2.2, 13.5**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn timer_a_interval_doubles_each_retransmission(
                t1_ms in 1u64..=2000,
                n in 0u32..=10,
            ) {
                let t1 = Duration::from_millis(t1_ms);
                let timer_config = TimerConfig {
                    t1,
                    t2: Duration::from_secs(4),
                    t4: Duration::from_secs(5),
                };
                let req = make_request(Method::Invite, "z9hG4bKtimerA");
                let id = TransactionId::from_request(&req).unwrap();
                let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
                let mut tx = InviteClientTransaction::new(id, req, dest, &timer_config);

                // 初期状態: timer_a_interval == T1
                prop_assert_eq!(tx.timer_a_interval, t1);

                // n回 on_timer_a() を呼び出す
                for _ in 0..n {
                    tx.on_timer_a();
                }

                // n回目の発火後、再送間隔は T1 * 2^n
                let expected_interval = t1 * 2u32.pow(n);
                prop_assert_eq!(
                    tx.timer_a_interval,
                    expected_interval,
                    "After {} retransmissions with T1={:?}: expected {:?}, got {:?}",
                    n,
                    t1,
                    expected_interval,
                    tx.timer_a_interval,
                );
            }
        }
    }
}
