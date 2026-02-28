use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;

use crate::error::SipLoadTestError;
use crate::sip::formatter::format_sip_message;
use crate::sip::message::{Method, SipMessage, SipRequest, SipResponse};
use crate::stats::StatsCollector;
use crate::transport::SipTransport;

use super::helpers::{extract_branch, generate_ack};
use super::invite_client::*;
use super::invite_server::*;
use super::non_invite_client::*;
use super::non_invite_server::*;
use super::timer::{TimerManager, TimerType};
use super::{TimerConfig, Transaction, TransactionEvent, TransactionId};

/// トランザクションのライフサイクルを管理する中心コンポーネント
/// DashMapベースの並行管理とTimerManagerによるタイマー管理を提供する
pub struct TransactionManager {
    pub(crate) transactions: DashMap<TransactionId, Transaction>,
    timer_config: TimerConfig,
    transport: Arc<dyn SipTransport>,
    stats: Arc<StatsCollector>,
    max_transactions: usize,
    pub(crate) timer_manager: TimerManager,
}

impl TransactionManager {
    /// 新しいTransactionManagerを作成する
    pub fn new(
        transport: Arc<dyn SipTransport>,
        stats: Arc<StatsCollector>,
        timer_config: TimerConfig,
        max_transactions: usize,
    ) -> Self {
        Self {
            transactions: DashMap::new(),
            timer_config,
            transport,
            stats,
            max_transactions,
            timer_manager: TimerManager::new(),
        }
    }

    /// クライアントトランザクション作成（リクエスト送信）
    /// リクエストメソッドに応じてINVITE/Non-INVITEクライアントトランザクションを作成し、
    /// DashMapに登録、タイマーをスケジュール、トランスポート経由で送信する
    pub async fn send_request(
        &self,
        request: SipRequest,
        destination: SocketAddr,
    ) -> Result<TransactionId, SipLoadTestError> {
        // max_transactions上限チェック
        if self.transactions.len() >= self.max_transactions {
            return Err(SipLoadTestError::MaxTransactionsReached(self.max_transactions));
        }

        let tx_id = TransactionId::from_request(&request)?;
        let now = Instant::now();

        let transaction = match request.method {
            Method::Invite => {
                let tx = InviteClientTransaction::new(
                    tx_id.clone(),
                    request.clone(),
                    destination,
                    &self.timer_config,
                );
                // Timer_A: 再送タイマー（初期値T1）
                self.timer_manager.schedule_timer_a(
                    now + self.timer_config.t1,
                    tx_id.clone(),
                );
                // Timer_B: タイムアウトタイマー（64*T1）
                self.timer_manager.schedule_timer_b(
                    now + self.timer_config.timer_b(),
                    tx_id.clone(),
                );
                Transaction::InviteClient(tx)
            }
            _ => {
                let tx = NonInviteClientTransaction::new(
                    tx_id.clone(),
                    request.clone(),
                    destination,
                    &self.timer_config,
                );
                // Timer_E: 再送タイマー（初期値T1）
                self.timer_manager.schedule_timer_e(
                    now + self.timer_config.t1,
                    tx_id.clone(),
                );
                // Timer_F: タイムアウトタイマー（64*T1）
                self.timer_manager.schedule_timer_f(
                    now + self.timer_config.timer_f(),
                    tx_id.clone(),
                );
                Transaction::NonInviteClient(tx)
            }
        };

        self.transactions.insert(tx_id.clone(), transaction);

        // トランスポート経由で送信
        let data = format_sip_message(&SipMessage::Request(request));
        self.transport.send_to(&data, destination).await?;

        Ok(tx_id)
    }

    /// レスポンス受信処理（対応するクライアントトランザクションにディスパッチ）
    pub fn handle_response(
        &self,
        response: &SipResponse,
    ) -> Option<TransactionEvent> {
        let tx_id = TransactionId::from_response(response).ok()?;

        let mut entry = self.transactions.get_mut(&tx_id)?;
        let status_code = response.status_code;

        match entry.value_mut() {
            Transaction::InviteClient(tx) => {
                let action = tx.on_response(status_code, &self.timer_config);
                tx.last_response = Some(response.clone());

                match action {
                    InviteClientAction::ResponseReceived => {
                        // 1xx暫定レスポンス → イベントとして返さない
                        None
                    }
                    InviteClientAction::Terminated => {
                        // 2xx最終レスポンス → Terminated
                        Some(TransactionEvent::Response(tx_id.clone(), response.clone()))
                    }
                    InviteClientAction::SendAck => {
                        // 3xx-6xx → Completed + ACK送信 + Timer_D
                        let ack = generate_ack(&tx.original_request, response);
                        let dest = tx.destination;
                        let data = format_sip_message(&SipMessage::Request(ack));
                        let transport = self.transport.clone();

                        // Timer_Dをスケジュール
                        let now = Instant::now();
                        self.timer_manager.schedule_timer_d(
                            now + self.timer_config.timer_d(),
                            tx_id.clone(),
                        );

                        // ACKを非同期で送信（ブロッキングを避けるためspawn）
                        // handle_responseは同期メソッドなので、tokio::spawnで送信
                        tokio::spawn(async move {
                            let _ = transport.send_to(&data, dest).await;
                        });

                        Some(TransactionEvent::Response(tx_id.clone(), response.clone()))
                    }
                    InviteClientAction::None => None,
                }
            }
            Transaction::NonInviteClient(tx) => {
                let action = tx.on_response(status_code, &self.timer_config);
                tx.last_response = Some(response.clone());

                match action {
                    NonInviteClientAction::ResponseReceived => {
                        // 2xx-6xx最終レスポンス → Completed + Timer_K
                        if status_code >= 200 {
                            let now = Instant::now();
                            self.timer_manager.schedule_timer_k(
                                now + self.timer_config.timer_k(),
                                tx_id.clone(),
                            );
                        }
                        Some(TransactionEvent::Response(tx_id.clone(), response.clone()))
                    }
                    NonInviteClientAction::Terminated => {
                        Some(TransactionEvent::Response(tx_id.clone(), response.clone()))
                    }
                    NonInviteClientAction::None => None,
                    NonInviteClientAction::Retransmit => None,
                }
            }
            // サーバートランザクションにはレスポンスをディスパッチしない
            _ => None,
        }
    }

    /// リクエスト受信処理（サーバートランザクション作成またはディスパッチ）
    pub fn handle_request(
        &self,
        request: &SipRequest,
        source: SocketAddr,
    ) -> Option<TransactionEvent> {
        // ACKの特別処理: INVITE サーバートランザクションのACK受信
        if request.method == Method::Ack {
            return self.handle_ack(request);
        }

        let tx_id = TransactionId::from_request(request).ok()?;

        // 既存トランザクションがあれば再送として処理
        if let Some(mut entry) = self.transactions.get_mut(&tx_id) {
            match entry.value_mut() {
                Transaction::InviteServer(tx) => {
                    let action = tx.on_request_retransmit();
                    match action {
                        InviteServerAction::RetransmitResponse(resp) => {
                            let data = format_sip_message(&SipMessage::Response(resp));
                            let transport = self.transport.clone();
                            let dest = source;
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });
                        }
                        InviteServerAction::SendProvisionalResponse(resp) => {
                            let data = format_sip_message(&SipMessage::Response(resp));
                            let transport = self.transport.clone();
                            let dest = source;
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });
                        }
                        _ => {}
                    }
                }
                Transaction::NonInviteServer(tx) => {
                    let action = tx.on_request_retransmit();
                    match action {
                        NonInviteServerAction::SendProvisionalResponse(resp) => {
                            let data = format_sip_message(&SipMessage::Response(resp));
                            let transport = self.transport.clone();
                            let dest = source;
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });
                        }
                        NonInviteServerAction::SendFinalResponse(resp) => {
                            let data = format_sip_message(&SipMessage::Response(resp));
                            let transport = self.transport.clone();
                            let dest = source;
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            // 再送は吸収（上位層に通知しない）
            return None;
        }

        // max_transactions上限チェック
        if self.transactions.len() >= self.max_transactions {
            return None;
        }

        // 新規サーバートランザクション作成
        let transaction = match request.method {
            Method::Invite => {
                let tx = InviteServerTransaction::new(
                    tx_id.clone(),
                    request.clone(),
                    source,
                    &self.timer_config,
                );
                Transaction::InviteServer(tx)
            }
            _ => {
                let tx = NonInviteServerTransaction::new(
                    tx_id.clone(),
                    request.clone(),
                    source,
                );
                Transaction::NonInviteServer(tx)
            }
        };

        self.transactions.insert(tx_id.clone(), transaction);
        Some(TransactionEvent::Request(tx_id, request.clone(), source))
    }

    /// ACK受信の処理
    /// 3xx-6xxに対するACKはINVITEサーバートランザクションで処理する
    fn handle_ack(&self, request: &SipRequest) -> Option<TransactionEvent> {
        // ACKのVia branchからINVITEトランザクションを検索
        let branch = request.headers.get("Via")
            .and_then(|via| extract_branch(via))?;

        let invite_tx_id = TransactionId {
            branch,
            method: Method::Invite,
        };

        if let Some(mut entry) = self.transactions.get_mut(&invite_tx_id) {
            if let Transaction::InviteServer(tx) = entry.value_mut() {
                let action = tx.on_ack_received(&self.timer_config);
                match action {
                    InviteServerAction::None => {
                        // Confirmed状態に遷移 → Timer_Iをスケジュール
                        if tx.state == InviteServerState::Confirmed {
                            let now = Instant::now();
                            self.timer_manager.schedule_timer_i(
                                now + self.timer_config.timer_i(),
                                invite_tx_id.clone(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        None
    }

    /// サーバートランザクション経由でレスポンス送信
    pub async fn send_response(
        &self,
        transaction_id: &TransactionId,
        response: SipResponse,
    ) -> Result<(), SipLoadTestError> {
        let mut entry = self.transactions.get_mut(transaction_id)
            .ok_or_else(|| SipLoadTestError::TransactionNotFound(
                format!("{:?}", transaction_id)
            ))?;

        let status_code = response.status_code;

        match entry.value_mut() {
            Transaction::InviteServer(tx) => {
                let source = tx.source;
                let action = tx.send_response(status_code, response.clone(), &self.timer_config);

                match action {
                    InviteServerAction::SendResponse => {
                        // 3xx-6xx → Completed: Timer_GとTimer_Hをスケジュール
                        if status_code >= 300 {
                            let now = Instant::now();
                            self.timer_manager.schedule_timer_g(
                                now + self.timer_config.t1,
                                transaction_id.clone(),
                            );
                            self.timer_manager.schedule_timer_h(
                                now + self.timer_config.timer_h(),
                                transaction_id.clone(),
                            );
                        }
                        let data = format_sip_message(&SipMessage::Response(response));
                        self.transport.send_to(&data, source).await?;
                    }
                    _ => {
                        // 2xx → Terminated: 送信のみ
                        if status_code >= 200 {
                            let data = format_sip_message(&SipMessage::Response(response));
                            self.transport.send_to(&data, source).await?;
                        }
                    }
                }
            }
            Transaction::NonInviteServer(tx) => {
                let source = tx.source;
                let action = tx.send_response(status_code, response.clone(), &self.timer_config);

                match action {
                    NonInviteServerAction::SendResponse => {
                        // 2xx-6xx → Completed: Timer_Jをスケジュール
                        if status_code >= 200 {
                            let now = Instant::now();
                            self.timer_manager.schedule_timer_j(
                                now + self.timer_config.timer_j(),
                                transaction_id.clone(),
                            );
                        }
                        let data = format_sip_message(&SipMessage::Response(response));
                        self.transport.send_to(&data, source).await?;
                    }
                    NonInviteServerAction::SendProvisionalResponse(_) => {
                        let data = format_sip_message(&SipMessage::Response(response));
                        self.transport.send_to(&data, source).await?;
                    }
                    NonInviteServerAction::SendFinalResponse(_) => {
                        // 2xx-6xx → Completed: Timer_Jをスケジュール
                        let now = Instant::now();
                        self.timer_manager.schedule_timer_j(
                            now + self.timer_config.timer_j(),
                            transaction_id.clone(),
                        );
                        let data = format_sip_message(&SipMessage::Response(response));
                        self.transport.send_to(&data, source).await?;
                    }
                    _ => {}
                }
            }
            _ => {
                return Err(SipLoadTestError::TransactionNotFound(
                    format!("{:?}", transaction_id)
                ));
            }
        }

        Ok(())
    }

    /// アクティブなトランザクション数を返す
    pub fn active_count(&self) -> usize {
        self.transactions.len()
    }

    /// タイマーティック処理（定期的に呼び出される）
    /// timer_manager.poll_expired(now) を呼び出して期限切れタイマーを取得し、
    /// 各ExpiredTimerについてDashMapでトランザクションを検索して状態遷移を実行する。
    pub async fn tick(&self) -> Vec<TransactionEvent> {
        let expired = self.timer_manager.poll_expired(Instant::now());
        let mut events = Vec::new();

        for entry in expired {
            let tx_id = entry.transaction_id;

            // DashMapでトランザクションを検索
            let mut tx_entry = match self.transactions.get_mut(&tx_id) {
                Some(e) => e,
                None => continue, // 遅延クリーンアップ: 存在しないトランザクションは破棄
            };

            // Terminated済みなら破棄
            if tx_entry.is_terminated() {
                drop(tx_entry);
                continue;
            }

            match entry.timer_type {
                // === 再送タイマー ===
                TimerType::TimerA => {
                    if let Transaction::InviteClient(tx) = tx_entry.value_mut() {
                        if tx.on_timer_a() {
                            let request = tx.original_request.clone();
                            let dest = tx.destination;
                            let new_interval = tx.timer_a_interval;
                            drop(tx_entry);

                            let data = format_sip_message(&SipMessage::Request(request));
                            let transport = self.transport.clone();
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });

                            self.timer_manager.schedule_timer_a(
                                Instant::now() + new_interval,
                                tx_id,
                            );
                            self.stats.record_retransmission();
                        }
                    }
                }
                TimerType::TimerE => {
                    if let Transaction::NonInviteClient(tx) = tx_entry.value_mut() {
                        if tx.on_timer_e(&self.timer_config) {
                            let request = tx.original_request.clone();
                            let dest = tx.destination;
                            let new_interval = tx.timer_e_interval;
                            drop(tx_entry);

                            let data = format_sip_message(&SipMessage::Request(request));
                            let transport = self.transport.clone();
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, dest).await;
                            });

                            self.timer_manager.schedule_timer_e(
                                Instant::now() + new_interval,
                                tx_id,
                            );
                            self.stats.record_retransmission();
                        }
                    }
                }
                TimerType::TimerG => {
                    if let Transaction::InviteServer(tx) = tx_entry.value_mut() {
                        let action = tx.on_timer_g(&self.timer_config);
                        if let InviteServerAction::RetransmitResponse(resp) = action {
                            let source = tx.source;
                            let new_interval = tx.timer_g_interval;
                            drop(tx_entry);

                            let data = format_sip_message(&SipMessage::Response(resp));
                            let transport = self.transport.clone();
                            tokio::spawn(async move {
                                let _ = transport.send_to(&data, source).await;
                            });

                            self.timer_manager.schedule_timer_g(
                                Instant::now() + new_interval,
                                tx_id,
                            );
                            self.stats.record_retransmission();
                        }
                    }
                }

                // === タイムアウトタイマー ===
                TimerType::TimerB => {
                    if let Transaction::InviteClient(tx) = tx_entry.value_mut() {
                        if tx.on_timer_b() {
                            drop(tx_entry);
                            self.stats.record_transaction_timeout();
                            events.push(TransactionEvent::Timeout(tx_id));
                        }
                    }
                }
                TimerType::TimerD => {
                    if let Transaction::InviteClient(tx) = tx_entry.value_mut() {
                        tx.on_timer_d();
                        // Timer_D: Terminated遷移のみ、タイムアウトイベントなし
                    }
                }
                TimerType::TimerF => {
                    if let Transaction::NonInviteClient(tx) = tx_entry.value_mut() {
                        if tx.on_timer_f() {
                            drop(tx_entry);
                            self.stats.record_transaction_timeout();
                            events.push(TransactionEvent::Timeout(tx_id));
                        }
                    }
                }
                TimerType::TimerH => {
                    if let Transaction::InviteServer(tx) = tx_entry.value_mut() {
                        let action = tx.on_timer_h();
                        if action == InviteServerAction::Timeout {
                            drop(tx_entry);
                            self.stats.record_transaction_timeout();
                            events.push(TransactionEvent::Timeout(tx_id));
                        }
                    }
                }
                TimerType::TimerI => {
                    if let Transaction::InviteServer(tx) = tx_entry.value_mut() {
                        tx.on_timer_i();
                        // Timer_I: Terminated遷移のみ、タイムアウトイベントなし
                    }
                }
                TimerType::TimerJ => {
                    if let Transaction::NonInviteServer(tx) = tx_entry.value_mut() {
                        tx.on_timer_j();
                        // Timer_J: Terminated遷移のみ、タイムアウトイベントなし
                    }
                }
                TimerType::TimerK => {
                    if let Transaction::NonInviteClient(tx) = tx_entry.value_mut() {
                        tx.on_timer_k();
                        // Timer_K: Terminated遷移のみ、タイムアウトイベントなし
                    }
                }
            }
        }

        events
    }

    /// Terminated状態のトランザクションをDashMapから削除する
    pub fn cleanup_terminated(&self) -> usize {
        let terminated_keys: Vec<TransactionId> = self
            .transactions
            .iter()
            .filter(|entry| entry.value().is_terminated())
            .map(|entry| entry.key().clone())
            .collect();

        let count = terminated_keys.len();
        for key in terminated_keys {
            self.transactions.remove(&key);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::sync::atomic::Ordering;

    use crate::sip::message::Headers;
    use crate::testutil::MockTransport;

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

    /// transaction 用の MockTransport ヘルパーメソッド拡張
    trait MockTransportTxExt {
        fn send_count(&self) -> usize;
    }

    impl MockTransportTxExt for MockTransport {
        fn send_count(&self) -> usize {
            self.send_count.load(Ordering::Relaxed)
        }
    }

    fn make_transaction_manager(max_transactions: usize) -> (TransactionManager, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let timer_config = TimerConfig::default();
        let mgr = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats,
            timer_config,
            max_transactions,
        );
        (mgr, transport)
    }

    #[test]
    fn transaction_manager_new_creates_empty_manager() {
        let (mgr, _transport) = make_transaction_manager(100);
        assert_eq!(mgr.active_count(), 0);
    }

    #[tokio::test]
    async fn transaction_manager_send_request_creates_invite_client_transaction() {
        let (mgr, transport) = make_transaction_manager(100);
        let req = make_request(Method::Invite, "z9hG4bK_mgr_inv");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();

        let tx_id = mgr.send_request(req, dest).await.unwrap();
        assert_eq!(tx_id.method, Method::Invite);
        assert_eq!(tx_id.branch, "z9hG4bK_mgr_inv");
        assert_eq!(mgr.active_count(), 1);
        assert_eq!(transport.send_count(), 1);
    }

    #[tokio::test]
    async fn transaction_manager_send_request_creates_non_invite_client_transaction() {
        let (mgr, transport) = make_transaction_manager(100);
        let req = make_request(Method::Register, "z9hG4bK_mgr_reg");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();

        let tx_id = mgr.send_request(req, dest).await.unwrap();
        assert_eq!(tx_id.method, Method::Register);
        assert_eq!(mgr.active_count(), 1);
        assert_eq!(transport.send_count(), 1);
    }

    #[tokio::test]
    async fn transaction_manager_send_request_respects_max_transactions() {
        let (mgr, _transport) = make_transaction_manager(1);
        let req1 = make_request(Method::Register, "z9hG4bK_max1");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req1, dest).await.unwrap();

        let req2 = make_request(Method::Register, "z9hG4bK_max2");
        let result = mgr.send_request(req2, dest).await;
        assert!(matches!(result, Err(SipLoadTestError::MaxTransactionsReached(1))));
    }

    #[tokio::test]
    async fn transaction_manager_handle_response_dispatches_to_invite_client() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Invite, "z9hG4bK_hr_inv");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req, dest).await.unwrap();

        // 200 OK → Terminated, TransactionEvent::Response
        let resp = make_response(200, "z9hG4bK_hr_inv", "INVITE");
        let event = mgr.handle_response(&resp);
        assert!(event.is_some());
        match event.unwrap() {
            TransactionEvent::Response(id, r) => {
                assert_eq!(id.branch, "z9hG4bK_hr_inv");
                assert_eq!(r.status_code, 200);
            }
            _ => panic!("Expected TransactionEvent::Response"),
        }
    }

    #[tokio::test]
    async fn transaction_manager_handle_response_dispatches_to_non_invite_client() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Register, "z9hG4bK_hr_reg");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req, dest).await.unwrap();

        // 200 OK → Completed, TransactionEvent::Response
        let resp = make_response(200, "z9hG4bK_hr_reg", "REGISTER");
        let event = mgr.handle_response(&resp);
        assert!(event.is_some());
        match event.unwrap() {
            TransactionEvent::Response(id, r) => {
                assert_eq!(id.branch, "z9hG4bK_hr_reg");
                assert_eq!(r.status_code, 200);
            }
            _ => panic!("Expected TransactionEvent::Response"),
        }
    }

    #[test]
    fn transaction_manager_handle_response_unknown_transaction_returns_none() {
        let (mgr, _transport) = make_transaction_manager(100);
        let resp = make_response(200, "z9hG4bK_unknown", "INVITE");
        let event = mgr.handle_response(&resp);
        assert!(event.is_none());
    }

    #[test]
    fn transaction_manager_handle_request_creates_invite_server_transaction() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Invite, "z9hG4bK_hreq_inv");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();

        let event = mgr.handle_request(&req, source);
        assert!(event.is_some());
        match event.unwrap() {
            TransactionEvent::Request(id, r, addr) => {
                assert_eq!(id.branch, "z9hG4bK_hreq_inv");
                assert_eq!(r.method, Method::Invite);
                assert_eq!(addr, source);
            }
            _ => panic!("Expected TransactionEvent::Request"),
        }
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn transaction_manager_handle_request_creates_non_invite_server_transaction() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Register, "z9hG4bK_hreq_reg");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();

        let event = mgr.handle_request(&req, source);
        assert!(event.is_some());
        match event.unwrap() {
            TransactionEvent::Request(id, _r, _addr) => {
                assert_eq!(id.branch, "z9hG4bK_hreq_reg");
            }
            _ => panic!("Expected TransactionEvent::Request"),
        }
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn transaction_manager_handle_request_absorbs_retransmit() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Register, "z9hG4bK_retrans");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();

        // 初回 → Request イベント
        let event1 = mgr.handle_request(&req, source);
        assert!(event1.is_some());

        // 再送 → 吸収（None）
        let event2 = mgr.handle_request(&req, source);
        assert!(event2.is_none());
        assert_eq!(mgr.active_count(), 1);
    }

    #[tokio::test]
    async fn transaction_manager_send_response_sends_via_server_transaction() {
        let (mgr, transport) = make_transaction_manager(100);
        let req = make_request(Method::Register, "z9hG4bK_sresp");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        mgr.handle_request(&req, source);

        let tx_id = TransactionId {
            branch: "z9hG4bK_sresp".to_string(),
            method: Method::Register,
        };
        let resp = make_response(200, "z9hG4bK_sresp", "REGISTER");
        let result = mgr.send_response(&tx_id, resp).await;
        assert!(result.is_ok());
        assert_eq!(transport.send_count(), 1);
    }

    #[tokio::test]
    async fn transaction_manager_send_response_not_found_returns_error() {
        let (mgr, _transport) = make_transaction_manager(100);
        let tx_id = TransactionId {
            branch: "z9hG4bK_notfound".to_string(),
            method: Method::Register,
        };
        let resp = make_response(200, "z9hG4bK_notfound", "REGISTER");
        let result = mgr.send_response(&tx_id, resp).await;
        assert!(matches!(result, Err(SipLoadTestError::TransactionNotFound(_))));
    }

    #[tokio::test]
    async fn transaction_manager_active_count_reflects_transactions() {
        let (mgr, _transport) = make_transaction_manager(100);
        assert_eq!(mgr.active_count(), 0);

        let req1 = make_request(Method::Invite, "z9hG4bK_ac1");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req1, dest).await.unwrap();
        assert_eq!(mgr.active_count(), 1);

        let req2 = make_request(Method::Register, "z9hG4bK_ac2");
        mgr.send_request(req2, dest).await.unwrap();
        assert_eq!(mgr.active_count(), 2);
    }

    #[tokio::test]
    async fn transaction_manager_handle_response_invite_3xx_sends_ack() {
        let (mgr, transport) = make_transaction_manager(100);
        let req = make_request(Method::Invite, "z9hG4bK_ack3xx");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req, dest).await.unwrap();

        // send_request で1回送信済み
        assert_eq!(transport.send_count(), 1);

        // 3xx → Completed + ACK送信
        let mut resp = make_response(300, "z9hG4bK_ack3xx", "INVITE");
        resp.headers.add("To", "sip:test@example.com;tag=resp-tag".to_string());
        let event = mgr.handle_response(&resp);
        assert!(event.is_some());
        // ACK送信はtokio::spawnで非同期実行されるため、yieldして完了を待つ
        tokio::task::yield_now().await;
        assert_eq!(transport.send_count(), 2);
    }

    #[tokio::test]
    async fn transaction_manager_handle_response_1xx_returns_none_for_invite() {
        let (mgr, _transport) = make_transaction_manager(100);
        let req = make_request(Method::Invite, "z9hG4bK_1xx");
        let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        mgr.send_request(req, dest).await.unwrap();

        // 1xx → Proceeding, None（暫定レスポンスはイベントとして返さない）
        let resp = make_response(100, "z9hG4bK_1xx", "INVITE");
        let event = mgr.handle_response(&resp);
        assert!(event.is_none());
    }

    #[test]
    fn transaction_manager_handle_request_respects_max_transactions() {
        let (mgr, _transport) = make_transaction_manager(1);
        let req1 = make_request(Method::Register, "z9hG4bK_hmax1");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        mgr.handle_request(&req1, source);

        let req2 = make_request(Method::Register, "z9hG4bK_hmax2");
        let event = mgr.handle_request(&req2, source);
        // 上限到達時はNoneを返す（エラーは内部で処理）
        assert!(event.is_none());
    }

    // === tick() / cleanup_terminated() tests ===

    fn make_transaction_manager_with_stats(
        max_transactions: usize,
    ) -> (TransactionManager, Arc<MockTransport>, Arc<StatsCollector>) {
        let transport = Arc::new(MockTransport::new());
        let stats = Arc::new(StatsCollector::new());
        let timer_config = TimerConfig::default();
        let mgr = TransactionManager::new(
            transport.clone() as Arc<dyn SipTransport>,
            stats.clone(),
            timer_config,
            max_transactions,
        );
        (mgr, transport, stats)
    }

    #[tokio::test]
    async fn tick_no_expired_timers_returns_empty() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let events = mgr.tick().await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn tick_timer_a_retransmits_invite_in_calling() {
        let (mgr, transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickA1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Directly insert an InviteClient transaction in Calling state
        let tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        // Schedule Timer_A with past deadline to simulate expiry
        mgr.timer_manager.schedule_timer_a(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        // Timer_A retransmit should not produce a TransactionEvent
        assert!(events.is_empty());
        // Allow spawned task to complete
        tokio::task::yield_now().await;
        // Should have sent one message (retransmit)
        assert_eq!(transport.send_count(), 1);
        // Stats should record retransmission
        let snap = stats.snapshot();
        assert_eq!(snap.retransmissions, 1);
    }

    #[tokio::test]
    async fn tick_timer_b_terminates_invite_client() {
        let (mgr, _transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickB1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        // Schedule Timer_B with past deadline
        mgr.timer_manager.schedule_timer_b(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            TransactionEvent::Timeout(id) => assert_eq!(id, &tx_id),
            _ => panic!("Expected Timeout event"),
        }
        // Stats should record timeout
        let snap = stats.snapshot();
        assert_eq!(snap.transaction_timeouts, 1);
        // Transaction should be terminated
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_timer_e_retransmits_non_invite_in_trying() {
        let (mgr, transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Register, "z9hG4bK_tickE1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = NonInviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::NonInviteClient(tx));

        // Schedule Timer_E with past deadline
        mgr.timer_manager.schedule_timer_e(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
        // Allow spawned task to complete
        tokio::task::yield_now().await;
        assert_eq!(transport.send_count(), 1);
        let snap = stats.snapshot();
        assert_eq!(snap.retransmissions, 1);
    }

    #[tokio::test]
    async fn tick_timer_f_terminates_non_invite_client() {
        let (mgr, _transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Register, "z9hG4bK_tickF1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = NonInviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::NonInviteClient(tx));

        mgr.timer_manager.schedule_timer_f(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            TransactionEvent::Timeout(id) => assert_eq!(id, &tx_id),
            _ => panic!("Expected Timeout event"),
        }
        let snap = stats.snapshot();
        assert_eq!(snap.transaction_timeouts, 1);
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_timer_g_retransmits_invite_server_response() {
        let (mgr, transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickG1");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create server transaction and move to Completed
        let mut tx = InviteServerTransaction::new(
            tx_id.clone(),
            req,
            source,
            &TimerConfig::default(),
        );
        let resp = make_response(486, "z9hG4bK_tickG1", "INVITE");
        tx.send_response(486, resp, &TimerConfig::default());
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteServer(tx));

        // Schedule Timer_G with past deadline
        mgr.timer_manager.schedule_timer_g(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
        // Allow spawned task to complete
        tokio::task::yield_now().await;
        assert_eq!(transport.send_count(), 1);
        let snap = stats.snapshot();
        assert_eq!(snap.retransmissions, 1);
    }

    #[tokio::test]
    async fn tick_timer_h_terminates_invite_server() {
        let (mgr, _transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickH1");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create server transaction and move to Completed
        let mut tx = InviteServerTransaction::new(
            tx_id.clone(),
            req,
            source,
            &TimerConfig::default(),
        );
        let resp = make_response(486, "z9hG4bK_tickH1", "INVITE");
        tx.send_response(486, resp, &TimerConfig::default());
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteServer(tx));

        mgr.timer_manager.schedule_timer_h(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            TransactionEvent::Timeout(id) => assert_eq!(id, &tx_id),
            _ => panic!("Expected Timeout event"),
        }
        let snap = stats.snapshot();
        assert_eq!(snap.transaction_timeouts, 1);
    }

    #[tokio::test]
    async fn tick_timer_d_terminates_invite_client_no_timeout_event() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickD1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create InviteClient in Completed state
        let mut tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        let resp = make_response(486, "z9hG4bK_tickD1", "INVITE");
        tx.on_response(486, &TimerConfig::default());
        tx.last_response = Some(resp);
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        mgr.timer_manager.schedule_timer_d(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        // Timer_D just terminates, no Timeout event
        assert!(events.is_empty());
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_timer_i_terminates_invite_server_no_timeout_event() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickI1");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create InviteServer in Confirmed state
        let mut tx = InviteServerTransaction::new(
            tx_id.clone(),
            req,
            source,
            &TimerConfig::default(),
        );
        let resp = make_response(486, "z9hG4bK_tickI1", "INVITE");
        tx.send_response(486, resp, &TimerConfig::default());
        tx.on_ack_received(&TimerConfig::default());
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteServer(tx));

        mgr.timer_manager.schedule_timer_i(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_timer_j_terminates_non_invite_server_no_timeout_event() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Register, "z9hG4bK_tickJ1");
        let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create NonInviteServer in Completed state
        let mut tx = NonInviteServerTransaction::new(
            tx_id.clone(),
            req,
            source,
        );
        let resp = make_response(200, "z9hG4bK_tickJ1", "REGISTER");
        tx.send_response(200, resp, &TimerConfig::default());
        mgr.transactions.insert(tx_id.clone(), Transaction::NonInviteServer(tx));

        mgr.timer_manager.schedule_timer_j(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_timer_k_terminates_non_invite_client_no_timeout_event() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Register, "z9hG4bK_tickK1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        // Create NonInviteClient in Completed state
        let mut tx = NonInviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        tx.on_response(200, &TimerConfig::default());
        mgr.transactions.insert(tx_id.clone(), Transaction::NonInviteClient(tx));

        mgr.timer_manager.schedule_timer_k(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
        let entry = mgr.transactions.get(&tx_id).unwrap();
        assert!(entry.is_terminated());
    }

    #[tokio::test]
    async fn tick_skips_nonexistent_transaction() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let fake_tx_id = TransactionId {
            branch: "z9hG4bK_ghost".to_string(),
            method: Method::Invite,
        };

        mgr.timer_manager.schedule_timer_a(
            Instant::now() - Duration::from_millis(10),
            fake_tx_id,
        );

        let events = mgr.tick().await;
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn tick_skips_terminated_transaction() {
        let (mgr, _transport, stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_tickTerm1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        // Terminate via Timer_B
        mgr.timer_manager.schedule_timer_b(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );
        mgr.tick().await;

        // Now schedule Timer_A for the already-terminated transaction
        mgr.timer_manager.schedule_timer_a(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );

        let snap_before = stats.snapshot();
        let events = mgr.tick().await;
        // Should skip the terminated transaction
        assert!(events.is_empty());
        // No additional retransmission recorded
        let snap_after = stats.snapshot();
        assert_eq!(snap_after.retransmissions, snap_before.retransmissions);
    }

    #[tokio::test]
    async fn cleanup_terminated_removes_terminated_transactions() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_cleanup1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        // Terminate via Timer_B
        mgr.timer_manager.schedule_timer_b(
            Instant::now() - Duration::from_millis(10),
            tx_id.clone(),
        );
        mgr.tick().await;

        assert_eq!(mgr.active_count(), 1); // still in DashMap
        let removed = mgr.cleanup_terminated();
        assert_eq!(removed, 1);
        assert_eq!(mgr.active_count(), 0);
    }

    #[test]
    fn cleanup_terminated_empty_manager_returns_zero() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let removed = mgr.cleanup_terminated();
        assert_eq!(removed, 0);
    }

    #[tokio::test]
    async fn cleanup_terminated_does_not_remove_active_transactions() {
        let (mgr, _transport, _stats) = make_transaction_manager_with_stats(100);
        let req = make_request(Method::Invite, "z9hG4bK_cleanupActive1");
        let dest: SocketAddr = "192.168.1.1:5060".parse().unwrap();
        let tx_id = TransactionId::from_request(&req).unwrap();

        let tx = InviteClientTransaction::new(
            tx_id.clone(),
            req,
            dest,
            &TimerConfig::default(),
        );
        mgr.transactions.insert(tx_id.clone(), Transaction::InviteClient(tx));

        assert_eq!(mgr.active_count(), 1);
        let removed = mgr.cleanup_terminated();
        assert_eq!(removed, 0);
        assert_eq!(mgr.active_count(), 1);
    }

    // === Property-based tests ===

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        // Feature: sip-transaction-layer, Property 13: トランザクション上限の強制
        // **Validates: Requirements 12.4**
        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn max_transactions_enforced_server(
                max_n in 1usize..=50,
            ) {
                let transport = Arc::new(MockTransport::new());
                let stats = Arc::new(StatsCollector::new());
                let timer_config = TimerConfig::default();
                let mgr = TransactionManager::new(
                    transport.clone() as Arc<dyn SipTransport>,
                    stats,
                    timer_config,
                    max_n,
                );
                let source: SocketAddr = "192.168.1.1:5060".parse().unwrap();

                // N個のサーバートランザクションを登録する
                for i in 0..max_n {
                    let branch = format!("z9hG4bKprop13s_{}", i);
                    let req = make_request(Method::Register, &branch);
                    let event = mgr.handle_request(&req, source);
                    prop_assert!(
                        event.is_some(),
                        "Transaction {} should be created (max={})", i, max_n,
                    );
                }

                prop_assert_eq!(mgr.active_count(), max_n);

                // N+1個目はNoneを返す（上限到達）
                let overflow_req = make_request(Method::Register, "z9hG4bKprop13s_overflow");
                let event = mgr.handle_request(&overflow_req, source);
                prop_assert!(
                    event.is_none(),
                    "Transaction beyond max_transactions={} should be rejected", max_n,
                );

                // アクティブ数はNのまま
                prop_assert_eq!(mgr.active_count(), max_n);
            }

            #[test]
            fn max_transactions_enforced_client(
                max_n in 1usize..=50,
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();

                rt.block_on(async {
                    let transport = Arc::new(MockTransport::new());
                    let stats = Arc::new(StatsCollector::new());
                    let timer_config = TimerConfig::default();
                    let mgr = TransactionManager::new(
                        transport.clone() as Arc<dyn SipTransport>,
                        stats,
                        timer_config,
                        max_n,
                    );
                    let dest: SocketAddr = "10.0.0.1:5060".parse().unwrap();

                    // N個のクライアントトランザクションを登録する
                    for i in 0..max_n {
                        let branch = format!("z9hG4bKprop13c_{}", i);
                        let req = make_request(Method::Register, &branch);
                        let result = mgr.send_request(req, dest).await;
                        assert!(
                            result.is_ok(),
                            "Transaction {} should be created (max={})", i, max_n,
                        );
                    }

                    assert_eq!(mgr.active_count(), max_n);

                    // N+1個目はMaxTransactionsReachedエラーを返す
                    let overflow_req = make_request(Method::Register, "z9hG4bKprop13c_overflow");
                    let result = mgr.send_request(overflow_req, dest).await;
                    assert!(
                        matches!(result, Err(SipLoadTestError::MaxTransactionsReached(n)) if n == max_n),
                        "Transaction beyond max_transactions={} should return MaxTransactionsReached error", max_n,
                    );

                    // アクティブ数はNのまま
                    assert_eq!(mgr.active_count(), max_n);
                });
            }
        }
    }
}
