pub mod timer;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::error::SipLoadTestError;
use crate::sip::formatter::format_sip_message;
use crate::sip::message::{Headers, Method, SipMessage, SipRequest, SipResponse};
use crate::stats::StatsCollector;
use crate::uas::SipTransport;

use self::timer::{TimerManager, TimerType};

/// RFC 3261準拠のトランザクション識別子
/// Via headerのbranchパラメータ + メソッドで一意に識別
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransactionId {
    pub branch: String,
    pub method: Method,
}

/// クライアントトランザクション（INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteClientState {
    Calling,
    Proceeding,
    Completed,
    Terminated,
}

/// クライアントトランザクション（Non-INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInviteClientState {
    Trying,
    Proceeding,
    Completed,
    Terminated,
}

/// サーバートランザクション（INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteServerState {
    Proceeding,
    Completed,
    Confirmed,
    Terminated,
}

/// サーバートランザクション（Non-INVITE）の状態
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonInviteServerState {
    Trying,
    Proceeding,
    Completed,
    Terminated,
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

/// Via headerからbranchパラメータを抽出する
fn extract_branch(via: &str) -> Option<String> {
    for part in via.split(';') {
        let trimmed = part.trim();
        if let Some(value) = trimmed.strip_prefix("branch=") {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// CSeq headerからメソッドを解析する
/// 形式: "1 INVITE" → Method::Invite
fn parse_method_from_cseq(cseq: &str) -> Option<Method> {
    let method_str = cseq.split_whitespace().nth(1)?;
    Some(match method_str {
        "REGISTER" => Method::Register,
        "INVITE" => Method::Invite,
        "ACK" => Method::Ack,
        "BYE" => Method::Bye,
        "OPTIONS" => Method::Options,
        "UPDATE" => Method::Update,
        other => Method::Other(other.to_string()),
    })
}


/// トランザクションのライフサイクルを管理する中心コンポーネント
/// DashMapベースの並行管理とTimerManagerによるタイマー管理を提供する
pub struct TransactionManager {
    transactions: DashMap<TransactionId, Transaction>,
    timer_config: TimerConfig,
    transport: Arc<dyn SipTransport>,
    stats: Arc<StatsCollector>,
    max_transactions: usize,
    timer_manager: TimerManager,
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

/// INVITE 3xx-6xxレスポンスに対するACKリクエストを生成する
fn generate_ack(original_request: &SipRequest, response: &SipResponse) -> SipRequest {
    let mut headers = Headers::new();

    // Via: 元のINVITEの最初のViaのみ
    if let Some(via) = original_request.headers.get("Via") {
        headers.add("Via", via.to_string());
    }

    // From: 元のINVITEと同じ
    if let Some(from) = original_request.headers.get("From") {
        headers.add("From", from.to_string());
    }

    // To: レスポンスから（tagを含む）
    if let Some(to) = response.headers.get("To") {
        headers.add("To", to.to_string());
    } else if let Some(to) = original_request.headers.get("To") {
        headers.add("To", to.to_string());
    }

    // Call-ID: 元のINVITEと同じ
    if let Some(call_id) = original_request.headers.get("Call-ID") {
        headers.add("Call-ID", call_id.to_string());
    }

    // CSeq: 同じシーケンス番号、メソッド = ACK
    if let Some(cseq) = original_request.headers.get("CSeq") {
        if let Some(seq_num) = cseq.split_whitespace().next() {
            headers.add("CSeq", format!("{} ACK", seq_num));
        }
    }

    // Content-Length: 0
    headers.add("Content-Length", "0".to_string());

    SipRequest {
        method: Method::Ack,
        request_uri: original_request.request_uri.clone(),
        version: "SIP/2.0".to_string(),
        headers,
        body: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::Headers;

    // === Helper functions ===

    fn make_request(method: Method, branch: &str) -> SipRequest {
        let mut headers = Headers::new();
        headers.add("Via", format!("SIP/2.0/UDP 192.168.1.1:5060;branch={}", branch));
        headers.add("CSeq", format!("1 {}", method_to_str(&method)));
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
    fn non_invite_client_state_variants() {
        let states = [
            NonInviteClientState::Trying,
            NonInviteClientState::Proceeding,
            NonInviteClientState::Completed,
            NonInviteClientState::Terminated,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j]);
            }
        }
    }

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

    // === extract_branch tests ===

    #[test]
    fn extract_branch_from_standard_via() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060;branch=z9hG4bK776asdhds";
        assert_eq!(extract_branch(via), Some("z9hG4bK776asdhds".to_string()));
    }

    #[test]
    fn extract_branch_with_multiple_params() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060;rport;branch=z9hG4bK001;received=10.0.0.1";
        assert_eq!(extract_branch(via), Some("z9hG4bK001".to_string()));
    }

    #[test]
    fn extract_branch_missing() {
        let via = "SIP/2.0/UDP 192.168.1.1:5060";
        assert_eq!(extract_branch(via), None);
    }

    // === parse_method_from_cseq tests ===

    #[test]
    fn parse_cseq_invite() {
        assert_eq!(parse_method_from_cseq("1 INVITE"), Some(Method::Invite));
    }

    #[test]
    fn parse_cseq_register() {
        assert_eq!(parse_method_from_cseq("1 REGISTER"), Some(Method::Register));
    }

    #[test]
    fn parse_cseq_bye() {
        assert_eq!(parse_method_from_cseq("2 BYE"), Some(Method::Bye));
    }

    #[test]
    fn parse_cseq_other_method() {
        assert_eq!(parse_method_from_cseq("1 SUBSCRIBE"), Some(Method::Other("SUBSCRIBE".to_string())));
    }

    #[test]
    fn parse_cseq_empty() {
        assert_eq!(parse_method_from_cseq(""), None);
    }

    #[test]
    fn parse_cseq_number_only() {
        assert_eq!(parse_method_from_cseq("1"), None);
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

    #[test]
    fn non_invite_server_transaction_initial_state() {
        let tx = make_non_invite_server_transaction("z9hG4bKnist001");
        assert_eq!(tx.state, NonInviteServerState::Trying);
        assert_eq!(tx.id.branch, "z9hG4bKnist001");
        assert_eq!(tx.id.method, Method::Register);
        assert!(tx.last_provisional_response.is_none());
        assert!(tx.last_final_response.is_none());
        assert!(tx.timer_j_deadline.is_none());
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

    // === InviteClientTransaction state transition tests ===

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

    // === Non-INVITE Client Transaction unit tests ===

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

    // === InviteServerTransaction unit tests ===

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
                let method_str = method_to_str(&method);
                let resp = make_response(200, &branch, method_str);

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

        /// INVITEクライアントトランザクションに適用するイベント
        #[derive(Debug, Clone)]
        enum InviteClientEvent {
            Response(u16),
            TimerA,
            TimerB,
            TimerD,
        }

        /// ランダムなINVITEクライアントイベントを生成するストラテジー
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

    // === Non-INVITEサーバートランザクション ユニットテスト ===

    // Non-INVITEサーバーイベント定義（プロパティテスト用）
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

    // === TransactionManager tests ===

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::pin::Pin;
    use std::future::Future;
    use crate::uas::SipTransport;
    use crate::stats::StatsCollector;

    /// テスト用のモックトランスポート
    struct MockTransport {
        send_count: AtomicUsize,
        sent_data: std::sync::Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                send_count: AtomicUsize::new(0),
                sent_data: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn send_count(&self) -> usize {
            self.send_count.load(Ordering::Relaxed)
        }
    }

    impl SipTransport for MockTransport {
        fn send_to<'a>(
            &'a self,
            data: &'a [u8],
            addr: SocketAddr,
        ) -> Pin<Box<dyn Future<Output = Result<(), crate::error::SipLoadTestError>> + Send + 'a>> {
            self.send_count.fetch_add(1, Ordering::Relaxed);
            self.sent_data.lock().unwrap().push((data.to_vec(), addr));
            Box::pin(async { Ok(()) })
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
}
