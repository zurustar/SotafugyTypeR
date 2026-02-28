pub mod timer;
mod helpers;
mod invite_client;
mod invite_server;
mod manager;
mod non_invite_client;
mod non_invite_server;

pub use helpers::{extract_branch, generate_ack, parse_method_from_cseq};
pub use invite_client::*;
pub use invite_server::*;
pub use manager::*;
pub use non_invite_client::*;
pub use non_invite_server::*;

use std::net::SocketAddr;
use std::time::Duration;

use crate::error::SipLoadTestError;
use crate::sip::message::{Method, SipRequest, SipResponse};

/// RFC 3261準拠のトランザクション識別子
/// Via headerのbranchパラメータ + メソッドで一意に識別
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransactionId {
    pub branch: String,
    pub method: Method,
}

/// SIPトランザクションタイマー設定
/// T1/T2/T4の値をカスタマイズ可能
#[derive(Debug, Clone)]
pub struct TimerConfig {
    pub t1: Duration,
    pub t2: Duration,
    pub t4: Duration,
}

impl Default for TimerConfig {
    fn default() -> Self {
        Self {
            t1: Duration::from_millis(500),
            t2: Duration::from_secs(4),
            t4: Duration::from_secs(5),
        }
    }
}

impl TimerConfig {
    /// Timer B: INVITE transaction timeout (64 * T1)
    pub fn timer_b(&self) -> Duration {
        self.t1 * 64
    }

    /// Timer D: Wait time in Completed state for INVITE client (max(32s, 64*T1))
    pub fn timer_d(&self) -> Duration {
        Duration::from_secs(32).max(self.t1 * 64)
    }

    /// Timer F: Non-INVITE transaction timeout (64 * T1)
    pub fn timer_f(&self) -> Duration {
        self.t1 * 64
    }

    /// Timer H: ACK wait timeout for INVITE server (64 * T1)
    pub fn timer_h(&self) -> Duration {
        self.t1 * 64
    }

    /// Timer I: Confirmed state wait for INVITE server (T4)
    pub fn timer_i(&self) -> Duration {
        self.t4
    }

    /// Timer J: Completed state wait for Non-INVITE server (64 * T1)
    pub fn timer_j(&self) -> Duration {
        self.t1 * 64
    }

    /// Timer K: Completed state wait for Non-INVITE client (T4)
    pub fn timer_k(&self) -> Duration {
        self.t4
    }
}

/// 4種類のトランザクションを統一的に扱うenum
pub enum Transaction {
    InviteClient(InviteClientTransaction),
    NonInviteClient(NonInviteClientTransaction),
    InviteServer(InviteServerTransaction),
    NonInviteServer(NonInviteServerTransaction),
}

impl Transaction {
    /// トランザクションがTerminated状態かどうかを返す
    pub fn is_terminated(&self) -> bool {
        match self {
            Transaction::InviteClient(tx) => tx.state == InviteClientState::Terminated,
            Transaction::NonInviteClient(tx) => tx.state == NonInviteClientState::Terminated,
            Transaction::InviteServer(tx) => tx.state == InviteServerState::Terminated,
            Transaction::NonInviteServer(tx) => tx.state == NonInviteServerState::Terminated,
        }
    }

    /// トランザクションIDへの参照を返す
    pub fn id(&self) -> &TransactionId {
        match self {
            Transaction::InviteClient(tx) => &tx.id,
            Transaction::NonInviteClient(tx) => &tx.id,
            Transaction::InviteServer(tx) => &tx.id,
            Transaction::NonInviteServer(tx) => &tx.id,
        }
    }
}

/// トランザクション層から上位層へ通知するイベント
pub enum TransactionEvent {
    /// 最終レスポンス受信（クライアントトランザクション）
    Response(TransactionId, SipResponse),
    /// 新規リクエスト受信（サーバートランザクション）
    Request(TransactionId, SipRequest, SocketAddr),
    /// トランザクションタイムアウト
    Timeout(TransactionId),
    /// トランスポートエラー
    TransportError(TransactionId, SipLoadTestError),
}

impl TransactionId {
    /// SIPリクエストからTransaction_IDを生成
    /// Via headerのbranchパラメータ + リクエストメソッド
    pub fn from_request(request: &SipRequest) -> Result<Self, SipLoadTestError> {
        let via = request
            .headers
            .get("Via")
            .ok_or_else(|| SipLoadTestError::ParseError("Missing Via header".to_string()))?;

        let branch = extract_branch(via)
            .ok_or_else(|| SipLoadTestError::ParseError("Missing branch parameter in Via header".to_string()))?;

        Ok(Self {
            branch,
            method: request.method.clone(),
        })
    }

    /// SIPレスポンスからTransaction_IDを特定
    /// Via headerのbranch + CSeq headerのメソッドを使用
    pub fn from_response(response: &SipResponse) -> Result<Self, SipLoadTestError> {
        let via = response
            .headers
            .get("Via")
            .ok_or_else(|| SipLoadTestError::ParseError("Missing Via header".to_string()))?;

        let branch = extract_branch(via)
            .ok_or_else(|| SipLoadTestError::ParseError("Missing branch parameter in Via header".to_string()))?;

        let cseq = response
            .headers
            .get("CSeq")
            .ok_or_else(|| SipLoadTestError::ParseError("Missing CSeq header".to_string()))?;

        let method = parse_method_from_cseq(cseq)
            .ok_or_else(|| SipLoadTestError::ParseError("Invalid CSeq header format".to_string()))?;

        Ok(Self { branch, method })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use crate::sip::message::Headers;

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

    // === TransactionId tests ===

    #[test]
    fn transaction_id_from_request_extracts_branch_and_method() {
        let req = make_request(Method::Invite, "z9hG4bK776asdhds");
        let id = TransactionId::from_request(&req).unwrap();
        assert_eq!(id.branch, "z9hG4bK776asdhds");
        assert_eq!(id.method, Method::Invite);
    }

    #[test]
    fn transaction_id_from_request_register() {
        let req = make_request(Method::Register, "z9hG4bKreg001");
        let id = TransactionId::from_request(&req).unwrap();
        assert_eq!(id.branch, "z9hG4bKreg001");
        assert_eq!(id.method, Method::Register);
    }

    #[test]
    fn transaction_id_from_response_extracts_branch_and_cseq_method() {
        let resp = make_response(200, "z9hG4bK776asdhds", "INVITE");
        let id = TransactionId::from_response(&resp).unwrap();
        assert_eq!(id.branch, "z9hG4bK776asdhds");
        assert_eq!(id.method, Method::Invite);
    }

    #[test]
    fn transaction_id_from_response_register() {
        let resp = make_response(200, "z9hG4bKreg001", "REGISTER");
        let id = TransactionId::from_response(&resp).unwrap();
        assert_eq!(id.branch, "z9hG4bKreg001");
        assert_eq!(id.method, Method::Register);
    }

    #[test]
    fn transaction_id_request_response_match() {
        let branch = "z9hG4bK776asdhds";
        let req = make_request(Method::Invite, branch);
        let resp = make_response(200, branch, "INVITE");
        let req_id = TransactionId::from_request(&req).unwrap();
        let resp_id = TransactionId::from_response(&resp).unwrap();
        assert_eq!(req_id, resp_id);
    }

    #[test]
    fn transaction_id_different_methods_are_different() {
        let branch = "z9hG4bKsame";
        let id1 = TransactionId { branch: branch.to_string(), method: Method::Invite };
        let id2 = TransactionId { branch: branch.to_string(), method: Method::Bye };
        assert_ne!(id1, id2);
    }

    #[test]
    fn transaction_id_from_request_missing_via_returns_error() {
        let req = SipRequest {
            method: Method::Invite,
            request_uri: "sip:test@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers: Headers::new(),
            body: None,
        };
        assert!(TransactionId::from_request(&req).is_err());
    }

    #[test]
    fn transaction_id_from_request_missing_branch_returns_error() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 192.168.1.1:5060".to_string());
        let req = SipRequest {
            method: Method::Invite,
            request_uri: "sip:test@example.com".to_string(),
            version: "SIP/2.0".to_string(),
            headers,
            body: None,
        };
        assert!(TransactionId::from_request(&req).is_err());
    }

    #[test]
    fn transaction_id_from_response_missing_via_returns_error() {
        let mut headers = Headers::new();
        headers.add("CSeq", "1 INVITE".to_string());
        let resp = SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        };
        assert!(TransactionId::from_response(&resp).is_err());
    }

    #[test]
    fn transaction_id_from_response_missing_cseq_returns_error() {
        let mut headers = Headers::new();
        headers.add("Via", "SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK001".to_string());
        let resp = SipResponse {
            version: "SIP/2.0".to_string(),
            status_code: 200,
            reason_phrase: "OK".to_string(),
            headers,
            body: None,
        };
        assert!(TransactionId::from_response(&resp).is_err());
    }

    #[test]
    fn transaction_id_hash_equality() {
        use std::collections::HashMap;
        let id1 = TransactionId { branch: "z9hG4bK001".to_string(), method: Method::Invite };
        let id2 = TransactionId { branch: "z9hG4bK001".to_string(), method: Method::Invite };
        let mut map = HashMap::new();
        map.insert(id1, "test");
        assert_eq!(map.get(&id2), Some(&"test"));
    }

    // === State enum tests ===

    #[test]
    fn invite_server_state_variants() {
        let states = [
            InviteServerState::Proceeding,
            InviteServerState::Completed,
            InviteServerState::Confirmed,
            InviteServerState::Terminated,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j]);
            }
        }
    }

    #[test]
    fn non_invite_server_state_variants() {
        let states = [
            NonInviteServerState::Trying,
            NonInviteServerState::Proceeding,
            NonInviteServerState::Completed,
            NonInviteServerState::Terminated,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j]);
            }
        }
    }

    #[test]
    fn state_enums_are_copy() {
        let s = InviteClientState::Calling;
        let s2 = s; // Copy
        assert_eq!(s, s2);

        let s = NonInviteClientState::Trying;
        let s2 = s;
        assert_eq!(s, s2);

        let s = InviteServerState::Proceeding;
        let s2 = s;
        assert_eq!(s, s2);

        let s = NonInviteServerState::Trying;
        let s2 = s;
        assert_eq!(s, s2);
    }

    #[test]
    fn state_enums_debug() {
        assert!(format!("{:?}", InviteClientState::Calling).contains("Calling"));
        assert!(format!("{:?}", NonInviteClientState::Trying).contains("Trying"));
        assert!(format!("{:?}", InviteServerState::Confirmed).contains("Confirmed"));
        assert!(format!("{:?}", NonInviteServerState::Completed).contains("Completed"));
    }

    // === TimerConfig tests ===

    #[test]
    fn timer_config_default_values() {
        let config = TimerConfig::default();
        assert_eq!(config.t1, Duration::from_millis(500));
        assert_eq!(config.t2, Duration::from_secs(4));
        assert_eq!(config.t4, Duration::from_secs(5));
    }

    #[test]
    fn timer_b_is_64_times_t1() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_b(), Duration::from_millis(500 * 64));
    }

    #[test]
    fn timer_d_is_at_least_32_seconds() {
        let config = TimerConfig::default();
        assert!(config.timer_d() >= Duration::from_secs(32));
    }

    #[test]
    fn timer_d_with_default_t1() {
        let config = TimerConfig::default();
        // 64 * 500ms = 32s, max(32s, 32s) = 32s
        assert_eq!(config.timer_d(), Duration::from_secs(32));
    }

    #[test]
    fn timer_d_with_large_t1() {
        let config = TimerConfig {
            t1: Duration::from_secs(1),
            t2: Duration::from_secs(4),
            t4: Duration::from_secs(5),
        };
        // 64 * 1s = 64s > 32s
        assert_eq!(config.timer_d(), Duration::from_secs(64));
    }

    #[test]
    fn timer_f_is_64_times_t1() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_f(), Duration::from_millis(500 * 64));
    }

    #[test]
    fn timer_h_is_64_times_t1() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_h(), Duration::from_millis(500 * 64));
    }

    #[test]
    fn timer_i_is_t4() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_i(), Duration::from_secs(5));
    }

    #[test]
    fn timer_j_is_64_times_t1() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_j(), Duration::from_millis(500 * 64));
    }

    #[test]
    fn timer_k_is_t4() {
        let config = TimerConfig::default();
        assert_eq!(config.timer_k(), Duration::from_secs(5));
    }

    #[test]
    fn timer_config_custom_values() {
        let config = TimerConfig {
            t1: Duration::from_millis(250),
            t2: Duration::from_secs(2),
            t4: Duration::from_secs(3),
        };
        assert_eq!(config.timer_b(), Duration::from_millis(250 * 64));
        assert_eq!(config.timer_f(), Duration::from_millis(250 * 64));
        assert_eq!(config.timer_h(), Duration::from_millis(250 * 64));
        assert_eq!(config.timer_j(), Duration::from_millis(250 * 64));
        assert_eq!(config.timer_i(), Duration::from_secs(3));
        assert_eq!(config.timer_k(), Duration::from_secs(3));
    }

    // === Transaction struct tests ===

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

    #[test]
    fn invite_server_transaction_initial_state() {
        let tx = make_invite_server_transaction("z9hG4bKist001");
        assert_eq!(tx.state, InviteServerState::Proceeding);
        assert_eq!(tx.id.branch, "z9hG4bKist001");
        assert_eq!(tx.id.method, Method::Invite);
        assert!(tx.last_provisional_response.is_none());
        assert!(tx.last_final_response.is_none());
        assert!(tx.timer_g_next.is_none());
        assert!(tx.timer_h_deadline.is_none());
        assert!(tx.timer_i_deadline.is_none());
        assert_eq!(tx.retransmit_count, 0);
    }

    // === Transaction enum tests ===

    #[test]
    fn transaction_enum_invite_client_id() {
        let tx = make_invite_client_transaction("z9hG4bKtxid001");
        let expected_id = tx.id.clone();
        let transaction = Transaction::InviteClient(tx);
        assert_eq!(transaction.id(), &expected_id);
    }

    #[test]
    fn transaction_enum_non_invite_client_id() {
        let tx = make_non_invite_client_transaction("z9hG4bKtxid002");
        let expected_id = tx.id.clone();
        let transaction = Transaction::NonInviteClient(tx);
        assert_eq!(transaction.id(), &expected_id);
    }

    #[test]
    fn transaction_enum_invite_server_id() {
        let tx = make_invite_server_transaction("z9hG4bKtxid003");
        let expected_id = tx.id.clone();
        let transaction = Transaction::InviteServer(tx);
        assert_eq!(transaction.id(), &expected_id);
    }

    #[test]
    fn transaction_enum_non_invite_server_id() {
        let tx = make_non_invite_server_transaction("z9hG4bKtxid004");
        let expected_id = tx.id.clone();
        let transaction = Transaction::NonInviteServer(tx);
        assert_eq!(transaction.id(), &expected_id);
    }

    #[test]
    fn transaction_is_terminated_false_for_active_states() {
        let ict = Transaction::InviteClient(make_invite_client_transaction("z9hG4bKterm001"));
        assert!(!ict.is_terminated());

        let nict = Transaction::NonInviteClient(make_non_invite_client_transaction("z9hG4bKterm002"));
        assert!(!nict.is_terminated());

        let ist = Transaction::InviteServer(make_invite_server_transaction("z9hG4bKterm003"));
        assert!(!ist.is_terminated());

        let nist = Transaction::NonInviteServer(make_non_invite_server_transaction("z9hG4bKterm004"));
        assert!(!nist.is_terminated());
    }

    #[test]
    fn transaction_is_terminated_true_for_terminated_states() {
        let mut ict = make_invite_client_transaction("z9hG4bKterm005");
        ict.state = InviteClientState::Terminated;
        assert!(Transaction::InviteClient(ict).is_terminated());

        let mut nict = make_non_invite_client_transaction("z9hG4bKterm006");
        nict.state = NonInviteClientState::Terminated;
        assert!(Transaction::NonInviteClient(nict).is_terminated());

        let mut ist = make_invite_server_transaction("z9hG4bKterm007");
        ist.state = InviteServerState::Terminated;
        assert!(Transaction::InviteServer(ist).is_terminated());

        let mut nist = make_non_invite_server_transaction("z9hG4bKterm008");
        nist.state = NonInviteServerState::Terminated;
        assert!(Transaction::NonInviteServer(nist).is_terminated());
    }

    // === TransactionEvent enum tests ===

    #[test]
    fn transaction_event_response_variant() {
        let id = TransactionId { branch: "z9hG4bKevt001".to_string(), method: Method::Invite };
        let resp = make_response(200, "z9hG4bKevt001", "INVITE");
        let event = TransactionEvent::Response(id.clone(), resp);
        assert!(matches!(event, TransactionEvent::Response(ref eid, _) if *eid == id));
    }

    #[test]
    fn transaction_event_request_variant() {
        let id = TransactionId { branch: "z9hG4bKevt002".to_string(), method: Method::Register };
        let req = make_request(Method::Register, "z9hG4bKevt002");
        let addr: std::net::SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let event = TransactionEvent::Request(id.clone(), req, addr);
        assert!(matches!(event, TransactionEvent::Request(ref eid, _, _) if *eid == id));
    }

    #[test]
    fn transaction_event_timeout_variant() {
        let id = TransactionId { branch: "z9hG4bKevt003".to_string(), method: Method::Invite };
        let event = TransactionEvent::Timeout(id.clone());
        assert!(matches!(event, TransactionEvent::Timeout(ref eid) if *eid == id));
    }

    #[test]
    fn transaction_event_transport_error_variant() {
        let id = TransactionId { branch: "z9hG4bKevt004".to_string(), method: Method::Invite };
        let err = SipLoadTestError::ParseError("test error".to_string());
        let event = TransactionEvent::TransportError(id.clone(), err);
        assert!(matches!(event, TransactionEvent::TransportError(ref eid, _) if *eid == id));
    }

    // === Property-based tests ===

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        /// Generate a random SIP method from the Method enum variants
        fn arb_method() -> impl Strategy<Value = Method> {
            prop_oneof![
                Just(Method::Register),
                Just(Method::Invite),
                Just(Method::Ack),
                Just(Method::Bye),
                Just(Method::Options),
                Just(Method::Update),
            ]
        }

        /// Generate a random branch parameter starting with "z9hG4bK"
        fn arb_branch() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9]{1,32}".prop_map(|suffix| format!("z9hG4bK{}", suffix))
        }

        // Feature: sip-transaction-layer, Property 1: Transaction_IDラウンドトリップ
        // **Validates: Requirements 1.1, 1.2**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn transaction_id_roundtrip(
                branch in arb_branch(),
                method in arb_method(),
            ) {
                let req = make_request(method.clone(), &branch);
                let method_str = method.to_string();
                let resp = make_response(200, &branch, &method_str);

                let req_id = TransactionId::from_request(&req).unwrap();
                let resp_id = TransactionId::from_response(&resp).unwrap();

                prop_assert_eq!(req_id, resp_id);
            }
        }

        // Feature: sip-transaction-layer, Property 2: 異なるメソッドは異なるTransaction_ID
        // **Validates: Requirements 1.4**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn different_methods_produce_different_transaction_ids(
                branch in arb_branch(),
                method1 in arb_method(),
                method2 in arb_method(),
            ) {
                prop_assume!(method1 != method2);

                let id1 = TransactionId {
                    branch: branch.clone(),
                    method: method1,
                };
                let id2 = TransactionId {
                    branch,
                    method: method2,
                };

                prop_assert_ne!(id1, id2);
            }
        }

        // Feature: sip-transaction-layer, Property 9: タイマー派生値の正当性
        // **Validates: Requirements 6.4, 6.5, 6.6**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn timer_derived_values_are_correct(
                t1_ms in 1u64..=2000,
                t2_ms in 1000u64..=10000,
                t4_ms in 1000u64..=10000,
            ) {
                let t1 = Duration::from_millis(t1_ms);
                let t2 = Duration::from_secs(t2_ms / 1000);
                let t4 = Duration::from_secs(t4_ms / 1000);

                let config = TimerConfig { t1, t2, t4 };

                // Timer_B = 64 * T1
                prop_assert_eq!(config.timer_b(), t1 * 64);
                // Timer_D >= 32s
                prop_assert!(config.timer_d() >= Duration::from_secs(32));
                // Timer_D = max(32s, 64 * T1)
                prop_assert_eq!(config.timer_d(), Duration::from_secs(32).max(t1 * 64));
                // Timer_F = 64 * T1
                prop_assert_eq!(config.timer_f(), t1 * 64);
                // Timer_H = 64 * T1
                prop_assert_eq!(config.timer_h(), t1 * 64);
                // Timer_I = T4
                prop_assert_eq!(config.timer_i(), t4);
                // Timer_J = 64 * T1
                prop_assert_eq!(config.timer_j(), t1 * 64);
                // Timer_K = T4
                prop_assert_eq!(config.timer_k(), t4);
            }
        }

    }

    // Non-INVITEサーバーイベント定義（共通プロパティテスト用）
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

    // === 全トランザクション共通プロパティテスト用のイベント型・ストラテジー ===

    /// INVITEクライアントトランザクション用イベント（共通プロパティテスト用）
    #[derive(Debug, Clone)]
    enum CommonInviteClientEvent {
        Response(u16),
        TimerA,
        TimerB,
        TimerD,
    }

    fn arb_common_invite_client_event() -> impl Strategy<Value = CommonInviteClientEvent> {
        prop_oneof![
            (100u16..700).prop_map(CommonInviteClientEvent::Response),
            Just(CommonInviteClientEvent::TimerA),
            Just(CommonInviteClientEvent::TimerB),
            Just(CommonInviteClientEvent::TimerD),
        ]
    }

    fn apply_common_invite_client_event(
        tx: &mut InviteClientTransaction,
        event: &CommonInviteClientEvent,
        timer_config: &TimerConfig,
    ) {
        match event {
            CommonInviteClientEvent::Response(code) => { tx.on_response(*code, timer_config); }
            CommonInviteClientEvent::TimerA => { tx.on_timer_a(); }
            CommonInviteClientEvent::TimerB => { tx.on_timer_b(); }
            CommonInviteClientEvent::TimerD => { tx.on_timer_d(); }
        }
    }

    /// Non-INVITEクライアントトランザクション用イベント（共通プロパティテスト用）
    #[derive(Debug, Clone)]
    enum CommonNonInviteClientEvent {
        Response(u16),
        TimerE,
        TimerF,
        TimerK,
    }

    fn arb_common_non_invite_client_event() -> impl Strategy<Value = CommonNonInviteClientEvent> {
        prop_oneof![
            (100u16..700).prop_map(CommonNonInviteClientEvent::Response),
            Just(CommonNonInviteClientEvent::TimerE),
            Just(CommonNonInviteClientEvent::TimerF),
            Just(CommonNonInviteClientEvent::TimerK),
        ]
    }

    fn apply_common_non_invite_client_event(
        tx: &mut NonInviteClientTransaction,
        event: &CommonNonInviteClientEvent,
        timer_config: &TimerConfig,
    ) {
        match event {
            CommonNonInviteClientEvent::Response(code) => { tx.on_response(*code, timer_config); }
            CommonNonInviteClientEvent::TimerE => { tx.on_timer_e(timer_config); }
            CommonNonInviteClientEvent::TimerF => { tx.on_timer_f(); }
            CommonNonInviteClientEvent::TimerK => { tx.on_timer_k(); }
        }
    }

    /// INVITEサーバートランザクション用イベント（共通プロパティテスト用）
    #[derive(Debug, Clone)]
    enum CommonInviteServerEvent {
        RequestRetransmit,
        SendResponse(u16),
        TimerG,
        TimerH,
        AckReceived,
        TimerI,
    }

    fn arb_common_invite_server_event() -> impl Strategy<Value = CommonInviteServerEvent> {
        prop_oneof![
            Just(CommonInviteServerEvent::RequestRetransmit),
            (200u16..700).prop_map(CommonInviteServerEvent::SendResponse),
            Just(CommonInviteServerEvent::TimerG),
            Just(CommonInviteServerEvent::TimerH),
            Just(CommonInviteServerEvent::AckReceived),
            Just(CommonInviteServerEvent::TimerI),
        ]
    }

    fn apply_common_invite_server_event(
        tx: &mut InviteServerTransaction,
        event: &CommonInviteServerEvent,
        timer_config: &TimerConfig,
    ) {
        match event {
            CommonInviteServerEvent::RequestRetransmit => { tx.on_request_retransmit(); }
            CommonInviteServerEvent::SendResponse(code) => {
                let resp = make_response(*code, &tx.id.branch, "INVITE");
                tx.send_response(*code, resp, timer_config);
            }
            CommonInviteServerEvent::TimerG => { tx.on_timer_g(timer_config); }
            CommonInviteServerEvent::TimerH => { tx.on_timer_h(); }
            CommonInviteServerEvent::AckReceived => { tx.on_ack_received(timer_config); }
            CommonInviteServerEvent::TimerI => { tx.on_timer_i(); }
        }
    }

    // Feature: sip-transaction-layer, Property 7: Terminated状態の不変性
    // **Validates: Requirements 13.4**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn terminated_invite_client_is_immutable(
            events in proptest::collection::vec(arb_common_invite_client_event(), 1..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Invite, "z9hG4bKterm_ic");
            let id = TransactionId::from_request(&req).unwrap();
            let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
            let mut tx = InviteClientTransaction::new(id, req, dest, &timer_config);
            tx.state = InviteClientState::Terminated;

            for event in &events {
                apply_common_invite_client_event(&mut tx, event, &timer_config);
                prop_assert_eq!(
                    tx.state,
                    InviteClientState::Terminated,
                    "INVITE client: Terminated state changed after event {:?}",
                    event,
                );
            }
        }

        #[test]
        fn terminated_non_invite_client_is_immutable(
            events in proptest::collection::vec(arb_common_non_invite_client_event(), 1..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Register, "z9hG4bKterm_nic");
            let id = TransactionId::from_request(&req).unwrap();
            let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
            let mut tx = NonInviteClientTransaction::new(id, req, dest, &timer_config);
            tx.state = NonInviteClientState::Terminated;

            for event in &events {
                apply_common_non_invite_client_event(&mut tx, event, &timer_config);
                prop_assert_eq!(
                    tx.state,
                    NonInviteClientState::Terminated,
                    "Non-INVITE client: Terminated state changed after event {:?}",
                    event,
                );
            }
        }

        #[test]
        fn terminated_invite_server_is_immutable(
            events in proptest::collection::vec(arb_common_invite_server_event(), 1..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Invite, "z9hG4bKterm_is");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = InviteServerTransaction::new(id, req, source, &timer_config);
            tx.state = InviteServerState::Terminated;

            for event in &events {
                apply_common_invite_server_event(&mut tx, event, &timer_config);
                prop_assert_eq!(
                    tx.state,
                    InviteServerState::Terminated,
                    "INVITE server: Terminated state changed after event {:?}",
                    event,
                );
            }
        }

        #[test]
        fn terminated_non_invite_server_is_immutable(
            events in proptest::collection::vec(arb_non_invite_server_event(), 1..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Register, "z9hG4bKterm_nis");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = NonInviteServerTransaction::new(id, req, source);
            tx.state = NonInviteServerState::Terminated;

            for event in &events {
                apply_non_invite_server_event(&mut tx, event, &timer_config);
                prop_assert_eq!(
                    tx.state,
                    NonInviteServerState::Terminated,
                    "Non-INVITE server: Terminated state changed after event {:?}",
                    event,
                );
            }
        }
    }

    // Feature: sip-transaction-layer, Property 8: 状態遷移の収束性（到達可能性）
    // **Validates: Requirements 13.1, 13.2, 13.3**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn invite_client_converges_to_terminated(
            events in proptest::collection::vec(arb_common_invite_client_event(), 0..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Invite, "z9hG4bKconv_ic");
            let id = TransactionId::from_request(&req).unwrap();
            let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
            let mut tx = InviteClientTransaction::new(id, req, dest, &timer_config);

            // ランダムなイベントシーケンスを適用
            for event in &events {
                apply_common_invite_client_event(&mut tx, event, &timer_config);
            }

            // タイムアウトタイマーを適用してTerminatedへ収束させる
            // Timer_B: Calling → Terminated
            // Timer_D: Completed → Terminated
            // 2xx: Calling/Proceeding → Terminated
            tx.on_timer_b();
            tx.on_response(200, &timer_config);
            tx.on_timer_d();

            prop_assert_eq!(
                tx.state,
                InviteClientState::Terminated,
                "INVITE client did not converge to Terminated after timeout timers",
            );
        }

        #[test]
        fn non_invite_client_converges_to_terminated(
            events in proptest::collection::vec(arb_common_non_invite_client_event(), 0..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Register, "z9hG4bKconv_nic");
            let id = TransactionId::from_request(&req).unwrap();
            let dest: SocketAddr = "127.0.0.1:5060".parse().unwrap();
            let mut tx = NonInviteClientTransaction::new(id, req, dest, &timer_config);

            // ランダムなイベントシーケンスを適用
            for event in &events {
                apply_common_non_invite_client_event(&mut tx, event, &timer_config);
            }

            // タイムアウトタイマーを適用してTerminatedへ収束させる
            // Timer_F: Trying/Proceeding → Terminated
            // Timer_K: Completed → Terminated
            tx.on_timer_f();
            tx.on_timer_k();

            prop_assert_eq!(
                tx.state,
                NonInviteClientState::Terminated,
                "Non-INVITE client did not converge to Terminated after timeout timers",
            );
        }

        #[test]
        fn invite_server_converges_to_terminated(
            events in proptest::collection::vec(arb_common_invite_server_event(), 0..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Invite, "z9hG4bKconv_is");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = InviteServerTransaction::new(id, req, source, &timer_config);

            // ランダムなイベントシーケンスを適用
            for event in &events {
                apply_common_invite_server_event(&mut tx, event, &timer_config);
            }

            // タイムアウトタイマーを適用してTerminatedへ収束させる
            // Timer_H: Completed → Terminated
            // Timer_I: Confirmed → Terminated
            // 2xx送信: Proceeding → Terminated
            let resp_2xx = make_response(200, "z9hG4bKconv_is", "INVITE");
            tx.send_response(200, resp_2xx, &timer_config);
            tx.on_timer_h();
            tx.on_timer_i();

            prop_assert_eq!(
                tx.state,
                InviteServerState::Terminated,
                "INVITE server did not converge to Terminated after timeout timers",
            );
        }

        #[test]
        fn non_invite_server_converges_to_terminated(
            events in proptest::collection::vec(arb_non_invite_server_event(), 0..20),
        ) {
            let timer_config = TimerConfig::default();
            let req = make_request(Method::Register, "z9hG4bKconv_nis");
            let id = TransactionId::from_request(&req).unwrap();
            let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
            let mut tx = NonInviteServerTransaction::new(id, req, source);

            // ランダムなイベントシーケンスを適用
            for event in &events {
                apply_non_invite_server_event(&mut tx, event, &timer_config);
            }

            // タイムアウトタイマーを適用してTerminatedへ収束させる
            // 2xx-6xx送信: Trying/Proceeding → Completed
            // Timer_J: Completed → Terminated
            let resp_200 = make_response(200, "z9hG4bKconv_nis", "REGISTER");
            tx.send_response(200, resp_200, &timer_config);
            tx.on_timer_j();

            prop_assert_eq!(
                tx.state,
                NonInviteServerState::Terminated,
                "Non-INVITE server did not converge to Terminated after timeout timers",
            );
        }
    }

    // Feature: sip-transaction-layer, Property 13: トランザクション上限の強制
    // **Validates: Requirements 12.4**
    // NOTE: max_transactions proptest は manager.rs に移動済み
}
