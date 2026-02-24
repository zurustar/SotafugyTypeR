use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use super::TransactionId;

/// タイマー種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerType {
    TimerA,
    TimerB,
    TimerD,
    TimerE,
    TimerF,
    TimerG,
    TimerH,
    TimerI,
    TimerJ,
    TimerK,
}

/// 期限切れタイマーエントリ
#[derive(Debug, Clone)]
pub struct ExpiredTimer {
    pub timer_type: TimerType,
    pub transaction_id: TransactionId,
    pub deadline: Instant,
}

/// タイマー種別ごとのVecDequeキューを管理する構造体
/// 10本のキュー（Timer_A〜Timer_K）を保持し、スケジュールとポーリングを提供する
pub struct TimerManager {
    timer_a_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_b_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_d_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_e_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_f_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_g_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_h_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_i_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_j_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
    timer_k_queue: Mutex<VecDeque<(Instant, TransactionId)>>,
}

impl TimerManager {
    /// 10本の空キューで初期化
    pub fn new() -> Self {
        Self {
            timer_a_queue: Mutex::new(VecDeque::new()),
            timer_b_queue: Mutex::new(VecDeque::new()),
            timer_d_queue: Mutex::new(VecDeque::new()),
            timer_e_queue: Mutex::new(VecDeque::new()),
            timer_f_queue: Mutex::new(VecDeque::new()),
            timer_g_queue: Mutex::new(VecDeque::new()),
            timer_h_queue: Mutex::new(VecDeque::new()),
            timer_i_queue: Mutex::new(VecDeque::new()),
            timer_j_queue: Mutex::new(VecDeque::new()),
            timer_k_queue: Mutex::new(VecDeque::new()),
        }
    }

    pub fn schedule_timer_a(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_a_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_b(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_b_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_d(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_d_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_e(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_e_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_f(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_f_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_g(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_g_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_h(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_h_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_i(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_i_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_j(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_j_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    pub fn schedule_timer_k(&self, deadline: Instant, tx_id: TransactionId) {
        self.timer_k_queue.lock().unwrap().push_back((deadline, tx_id));
    }

    /// 全10キューの先頭を確認し、deadline <= now のエントリをpopして返す
    /// 各キューについて、先頭が期限切れでなくなるまでpopを繰り返す
    pub fn poll_expired(&self, now: Instant) -> Vec<ExpiredTimer> {
        let mut expired = Vec::new();

        self.drain_queue(&self.timer_a_queue, TimerType::TimerA, now, &mut expired);
        self.drain_queue(&self.timer_b_queue, TimerType::TimerB, now, &mut expired);
        self.drain_queue(&self.timer_d_queue, TimerType::TimerD, now, &mut expired);
        self.drain_queue(&self.timer_e_queue, TimerType::TimerE, now, &mut expired);
        self.drain_queue(&self.timer_f_queue, TimerType::TimerF, now, &mut expired);
        self.drain_queue(&self.timer_g_queue, TimerType::TimerG, now, &mut expired);
        self.drain_queue(&self.timer_h_queue, TimerType::TimerH, now, &mut expired);
        self.drain_queue(&self.timer_i_queue, TimerType::TimerI, now, &mut expired);
        self.drain_queue(&self.timer_j_queue, TimerType::TimerJ, now, &mut expired);
        self.drain_queue(&self.timer_k_queue, TimerType::TimerK, now, &mut expired);

        expired
    }

    /// キューの先頭から期限切れエントリをpopするヘルパー
    fn drain_queue(
        &self,
        queue: &Mutex<VecDeque<(Instant, TransactionId)>>,
        timer_type: TimerType,
        now: Instant,
        expired: &mut Vec<ExpiredTimer>,
    ) {
        let mut q = queue.lock().unwrap();
        while let Some(&(deadline, _)) = q.front() {
            if deadline <= now {
                let (deadline, tx_id) = q.pop_front().unwrap();
                expired.push(ExpiredTimer {
                    timer_type,
                    transaction_id: tx_id,
                    deadline,
                });
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sip::message::Method;
    use std::time::{Duration, Instant};

    fn make_tx_id(branch: &str, method: Method) -> TransactionId {
        TransactionId {
            branch: branch.to_string(),
            method,
        }
    }

    // --- schedule + poll テスト ---

    #[test]
    fn schedule_timer_a_and_poll_returns_expired() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let deadline = now - Duration::from_millis(10); // 過去の期限
        let tx_id = make_tx_id("z9hG4bK-test1", Method::Invite);

        mgr.schedule_timer_a(deadline, tx_id.clone());

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].timer_type, TimerType::TimerA);
        assert_eq!(expired[0].transaction_id, tx_id);
        assert_eq!(expired[0].deadline, deadline);
    }

    #[test]
    fn schedule_timer_b_and_poll_returns_expired() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let deadline = now - Duration::from_millis(5);
        let tx_id = make_tx_id("z9hG4bK-test2", Method::Invite);

        mgr.schedule_timer_b(deadline, tx_id.clone());

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].timer_type, TimerType::TimerB);
        assert_eq!(expired[0].transaction_id, tx_id);
    }

    #[test]
    fn schedule_all_timer_types_and_poll() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let past = now - Duration::from_millis(10);

        mgr.schedule_timer_a(past, make_tx_id("a", Method::Invite));
        mgr.schedule_timer_b(past, make_tx_id("b", Method::Invite));
        mgr.schedule_timer_d(past, make_tx_id("d", Method::Invite));
        mgr.schedule_timer_e(past, make_tx_id("e", Method::Register));
        mgr.schedule_timer_f(past, make_tx_id("f", Method::Register));
        mgr.schedule_timer_g(past, make_tx_id("g", Method::Invite));
        mgr.schedule_timer_h(past, make_tx_id("h", Method::Invite));
        mgr.schedule_timer_i(past, make_tx_id("i", Method::Invite));
        mgr.schedule_timer_j(past, make_tx_id("j", Method::Register));
        mgr.schedule_timer_k(past, make_tx_id("k", Method::Register));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 10);

        // 各タイマー種別が1つずつ含まれることを確認
        let types: Vec<TimerType> = expired.iter().map(|e| e.timer_type).collect();
        assert!(types.contains(&TimerType::TimerA));
        assert!(types.contains(&TimerType::TimerB));
        assert!(types.contains(&TimerType::TimerD));
        assert!(types.contains(&TimerType::TimerE));
        assert!(types.contains(&TimerType::TimerF));
        assert!(types.contains(&TimerType::TimerG));
        assert!(types.contains(&TimerType::TimerH));
        assert!(types.contains(&TimerType::TimerI));
        assert!(types.contains(&TimerType::TimerJ));
        assert!(types.contains(&TimerType::TimerK));
    }

    // --- 期限切れ順序テスト ---

    #[test]
    fn poll_returns_entries_in_insertion_order() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let d1 = now - Duration::from_millis(30);
        let d2 = now - Duration::from_millis(20);
        let d3 = now - Duration::from_millis(10);

        mgr.schedule_timer_a(d1, make_tx_id("first", Method::Invite));
        mgr.schedule_timer_a(d2, make_tx_id("second", Method::Invite));
        mgr.schedule_timer_a(d3, make_tx_id("third", Method::Invite));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 3);
        assert_eq!(expired[0].transaction_id.branch, "first");
        assert_eq!(expired[1].transaction_id.branch, "second");
        assert_eq!(expired[2].transaction_id.branch, "third");
    }

    #[test]
    fn poll_only_returns_expired_entries_not_future() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let past = now - Duration::from_millis(10);
        let future = now + Duration::from_secs(10);

        mgr.schedule_timer_a(past, make_tx_id("expired", Method::Invite));
        mgr.schedule_timer_a(future, make_tx_id("not-expired", Method::Invite));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].transaction_id.branch, "expired");
    }

    #[test]
    fn poll_stops_at_first_non_expired_entry() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let past1 = now - Duration::from_millis(20);
        let past2 = now - Duration::from_millis(10);
        let future = now + Duration::from_secs(5);

        mgr.schedule_timer_e(past1, make_tx_id("e1", Method::Register));
        mgr.schedule_timer_e(past2, make_tx_id("e2", Method::Register));
        mgr.schedule_timer_e(future, make_tx_id("e3", Method::Register));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 2);
        assert_eq!(expired[0].transaction_id.branch, "e1");
        assert_eq!(expired[1].transaction_id.branch, "e2");

        // future エントリはまだキューに残っている
        let future_now = future + Duration::from_millis(1);
        let expired2 = mgr.poll_expired(future_now);
        assert_eq!(expired2.len(), 1);
        assert_eq!(expired2[0].transaction_id.branch, "e3");
    }

    // --- 空poll テスト ---

    #[test]
    fn poll_empty_manager_returns_empty_vec() {
        let mgr = TimerManager::new();
        let expired = mgr.poll_expired(Instant::now());
        assert!(expired.is_empty());
    }

    #[test]
    fn poll_with_only_future_entries_returns_empty() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let future = now + Duration::from_secs(60);

        mgr.schedule_timer_a(future, make_tx_id("future-a", Method::Invite));
        mgr.schedule_timer_f(future, make_tx_id("future-f", Method::Register));

        let expired = mgr.poll_expired(now);
        assert!(expired.is_empty());
    }

    #[test]
    fn poll_removes_expired_entries_from_queue() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let past = now - Duration::from_millis(10);

        mgr.schedule_timer_a(past, make_tx_id("once", Method::Invite));

        let expired1 = mgr.poll_expired(now);
        assert_eq!(expired1.len(), 1);

        // 2回目のpollでは空になる
        let expired2 = mgr.poll_expired(now);
        assert!(expired2.is_empty());
    }

    #[test]
    fn poll_exact_deadline_is_expired() {
        let mgr = TimerManager::new();
        let now = Instant::now();

        // deadline == now の場合も期限切れとして扱う（deadline <= now）
        mgr.schedule_timer_b(now, make_tx_id("exact", Method::Invite));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].transaction_id.branch, "exact");
    }

    // --- 複数キュー同時テスト ---

    #[test]
    fn multiple_queues_polled_independently() {
        let mgr = TimerManager::new();
        let now = Instant::now();
        let past = now - Duration::from_millis(10);
        let future = now + Duration::from_secs(10);

        // Timer_A: 期限切れ
        mgr.schedule_timer_a(past, make_tx_id("a-expired", Method::Invite));
        // Timer_B: 未来
        mgr.schedule_timer_b(future, make_tx_id("b-future", Method::Invite));
        // Timer_E: 期限切れ
        mgr.schedule_timer_e(past, make_tx_id("e-expired", Method::Register));

        let expired = mgr.poll_expired(now);
        assert_eq!(expired.len(), 2);

        let types: Vec<TimerType> = expired.iter().map(|e| e.timer_type).collect();
        assert!(types.contains(&TimerType::TimerA));
        assert!(types.contains(&TimerType::TimerE));
        assert!(!types.contains(&TimerType::TimerB));
    }
}
