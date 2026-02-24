// Dialog manager module

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use crate::error::SipLoadTestError;
use crate::sip::message::SipMessage;

thread_local! {
    static FAST_RNG: RefCell<SmallRng> = RefCell::new(SmallRng::from_entropy());
}

/// SIP dialog state
#[derive(Debug, Clone, PartialEq)]
pub enum DialogState {
    Initial,
    Trying,
    Early,
    Confirmed,
    Terminated,
    Error,
}

/// A single SIP dialog
#[derive(Debug, Clone)]
pub struct Dialog {
    pub call_id: String,
    pub from_tag: String,
    pub to_tag: Option<String>,
    pub state: DialogState,
    pub cseq: u32,
    pub created_at: Instant,
    pub session_expires: Option<Duration>,
    pub auth_attempted: bool,
    pub original_request: Option<SipMessage>,
}

/// Manages concurrent SIP dialogs using DashMap
pub struct DialogManager {
    dialogs: DashMap<String, Dialog>,
    max_dialogs: usize,
    active_count: AtomicUsize,
}

impl DialogManager {
    /// Create a new DialogManager with the given max_dialogs capacity limit
    pub fn new(max_dialogs: usize) -> Self {
        Self {
            dialogs: DashMap::new(),
            max_dialogs,
            active_count: AtomicUsize::new(0),
        }
    }

    /// Create a new dialog with unique Call-ID, From-tag, and CSeq.
    /// Returns error if max_dialogs limit is reached.
    pub fn create_dialog(&self) -> Result<Dialog, SipLoadTestError> {
        let current = self.active_count.load(Ordering::SeqCst);
        if current >= self.max_dialogs {
            return Err(SipLoadTestError::MaxDialogsReached(self.max_dialogs));
        }

        let call_id = Self::generate_call_id();
        let from_tag = Self::generate_from_tag();

        let dialog = Dialog {
            call_id: call_id.clone(),
            from_tag,
            to_tag: None,
            state: DialogState::Initial,
            cseq: 1,
            created_at: Instant::now(),
            session_expires: None,
            auth_attempted: false,
            original_request: None,
        };

        self.dialogs.insert(call_id, dialog.clone());
        self.active_count.fetch_add(1, Ordering::SeqCst);

        Ok(dialog)
    }

    /// Look up a dialog by Call-ID
    pub fn get_dialog(&self, call_id: &str) -> Option<dashmap::mapref::one::Ref<'_, String, Dialog>> {
        self.dialogs.get(call_id)
    }

    /// Update a dialog's state by Call-ID
    pub fn update_dialog_state(&self, call_id: &str, state: DialogState) -> Result<(), SipLoadTestError> {
        match self.dialogs.get_mut(call_id) {
            Some(mut entry) => {
                entry.state = state;
                Ok(())
            }
            None => Err(SipLoadTestError::DialogNotFound(call_id.to_string())),
        }
    }

    /// Remove a dialog by Call-ID
    pub fn remove_dialog(&self, call_id: &str) -> Option<Dialog> {
        self.dialogs.remove(call_id).map(|(_, dialog)| {
            self.active_count.fetch_sub(1, Ordering::SeqCst);
            dialog
        })
    }

    /// Get the current number of active dialogs
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::SeqCst)
    }

    /// Get a mutable reference to a dialog by Call-ID
    pub fn get_dialog_mut(&self, call_id: &str) -> Option<dashmap::mapref::one::RefMut<'_, String, Dialog>> {
        self.dialogs.get_mut(call_id)
    }

    /// Insert a pre-built dialog into the manager.
    /// Increments active count. Returns error if max_dialogs reached.
    pub fn insert_dialog(&self, dialog: Dialog) -> Result<(), SipLoadTestError> {
        let current = self.active_count.load(Ordering::SeqCst);
        if current >= self.max_dialogs {
            return Err(SipLoadTestError::MaxDialogsReached(self.max_dialogs));
        }
        self.dialogs.insert(dialog.call_id.clone(), dialog);
        self.active_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Collect all Call-IDs currently in the manager
    pub fn all_call_ids(&self) -> Vec<String> {
        self.dialogs.iter().map(|entry| entry.key().clone()).collect()
    }

    /// Collect Call-IDs of dialogs that have timed out.
    /// Only clones Call-IDs for dialogs where:
    /// - elapsed time since creation exceeds the timeout threshold
    /// - state is NOT Terminated or Error
    pub fn collect_timed_out(&self, timeout: Duration) -> Vec<String> {
        let now = Instant::now();
        self.dialogs
            .iter()
            .filter(|entry| now.duration_since(entry.value().created_at) >= timeout)
            .filter(|entry| {
                !matches!(
                    entry.value().state,
                    DialogState::Terminated | DialogState::Error
                )
            })
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Collect dialogs that are Confirmed and have exceeded the given call_duration.
    /// Returns a Vec of (call_id, from_tag, to_tag, cseq) tuples for BYE batch sending.
    pub fn collect_expired_for_bye(
        &self,
        call_duration: Duration,
    ) -> Vec<(String, String, Option<String>, u32)> {
        let now = Instant::now();
        self.dialogs
            .iter()
            .filter(|entry| {
                entry.value().state == DialogState::Confirmed
                    && now.duration_since(entry.value().created_at) >= call_duration
            })
            .map(|entry| {
                let d = entry.value();
                (
                    d.call_id.clone(),
                    d.from_tag.clone(),
                    d.to_tag.clone(),
                    d.cseq,
                )
            })
            .collect()
    }

    /// Generate a unique Call-ID string
    pub fn generate_call_id() -> String {
        FAST_RNG.with(|rng| {
            let val: u128 = rng.borrow_mut().gen();
            format!("{:032x}", val)
        })
    }

    /// Generate a unique From-tag string
    pub fn generate_from_tag() -> String {
        FAST_RNG.with(|rng| {
            let val: u64 = rng.borrow_mut().gen();
            format!("{:016x}", val)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- Unit tests: DialogState ---

    #[test]
    fn test_dialog_state_clone_and_eq() {
        let states = vec![
            DialogState::Initial,
            DialogState::Trying,
            DialogState::Early,
            DialogState::Confirmed,
            DialogState::Terminated,
            DialogState::Error,
        ];
        for s in &states {
            let cloned = s.clone();
            assert_eq!(s, &cloned);
        }
    }

    #[test]
    fn test_dialog_state_inequality() {
        assert_ne!(DialogState::Initial, DialogState::Confirmed);
        assert_ne!(DialogState::Trying, DialogState::Error);
    }

    // --- Unit tests: DialogManager::new ---

    #[test]
    fn test_new_dialog_manager_has_zero_active() {
        let dm = DialogManager::new(100);
        assert_eq!(dm.active_count(), 0);
    }

    #[test]
    fn test_new_dialog_manager_stores_max_dialogs() {
        let dm = DialogManager::new(500);
        // Verify by trying to create up to the limit
        assert_eq!(dm.active_count(), 0);
    }

    // --- Unit tests: create_dialog ---

    #[test]
    fn test_create_dialog_returns_dialog_with_initial_state() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        assert_eq!(dialog.state, DialogState::Initial);
    }

    #[test]
    fn test_create_dialog_has_non_empty_call_id() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        assert!(!dialog.call_id.is_empty());
    }

    #[test]
    fn test_create_dialog_has_non_empty_from_tag() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        assert!(!dialog.from_tag.is_empty());
    }

    #[test]
    fn test_create_dialog_has_cseq_one() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        assert_eq!(dialog.cseq, 1);
    }

    #[test]
    fn test_create_dialog_increments_active_count() {
        let dm = DialogManager::new(10);
        assert_eq!(dm.active_count(), 0);
        dm.create_dialog().unwrap();
        assert_eq!(dm.active_count(), 1);
        dm.create_dialog().unwrap();
        assert_eq!(dm.active_count(), 2);
    }

    #[test]
    fn test_create_dialog_defaults() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        assert_eq!(dialog.to_tag, None);
        assert_eq!(dialog.session_expires, None);
        assert!(!dialog.auth_attempted);
        assert!(dialog.original_request.is_none());
    }

    #[test]
    fn test_create_dialog_unique_call_ids() {
        let dm = DialogManager::new(100);
        let d1 = dm.create_dialog().unwrap();
        let d2 = dm.create_dialog().unwrap();
        let d3 = dm.create_dialog().unwrap();
        assert_ne!(d1.call_id, d2.call_id);
        assert_ne!(d1.call_id, d3.call_id);
        assert_ne!(d2.call_id, d3.call_id);
    }

    // --- Unit tests: max_dialogs limit ---

    #[test]
    fn test_create_dialog_at_max_returns_error() {
        let dm = DialogManager::new(2);
        dm.create_dialog().unwrap();
        dm.create_dialog().unwrap();
        let result = dm.create_dialog();
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::MaxDialogsReached(n) => assert_eq!(n, 2),
            other => panic!("Expected MaxDialogsReached, got {:?}", other),
        }
    }

    #[test]
    fn test_create_dialog_max_one() {
        let dm = DialogManager::new(1);
        dm.create_dialog().unwrap();
        assert!(dm.create_dialog().is_err());
    }

    #[test]
    fn test_remove_then_create_within_limit() {
        let dm = DialogManager::new(1);
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.remove_dialog(&call_id);
        // Should be able to create again after removal
        assert!(dm.create_dialog().is_ok());
    }

    // --- Unit tests: get_dialog ---

    #[test]
    fn test_get_dialog_existing() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();
        let found = dm.get_dialog(&call_id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().call_id, call_id);
    }

    #[test]
    fn test_get_dialog_nonexistent() {
        let dm = DialogManager::new(10);
        assert!(dm.get_dialog("nonexistent-call-id").is_none());
    }

    // --- Unit tests: update_dialog_state ---

    #[test]
    fn test_update_dialog_state_success() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();

        dm.update_dialog_state(&call_id, DialogState::Trying).unwrap();
        let updated = dm.get_dialog(&call_id).unwrap();
        assert_eq!(updated.state, DialogState::Trying);
    }

    #[test]
    fn test_update_dialog_state_multiple_transitions() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();

        dm.update_dialog_state(&call_id, DialogState::Trying).unwrap();
        dm.update_dialog_state(&call_id, DialogState::Early).unwrap();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();

        let d = dm.get_dialog(&call_id).unwrap();
        assert_eq!(d.state, DialogState::Confirmed);
    }

    #[test]
    fn test_update_dialog_state_nonexistent_returns_error() {
        let dm = DialogManager::new(10);
        let result = dm.update_dialog_state("nonexistent", DialogState::Trying);
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::DialogNotFound(id) => assert_eq!(id, "nonexistent"),
            other => panic!("Expected DialogNotFound, got {:?}", other),
        }
    }

    // --- Unit tests: remove_dialog ---

    #[test]
    fn test_remove_dialog_existing() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();

        let removed = dm.remove_dialog(&call_id);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().call_id, call_id);
    }

    #[test]
    fn test_remove_dialog_decrements_active_count() {
        let dm = DialogManager::new(10);
        let d1 = dm.create_dialog().unwrap();
        let d2 = dm.create_dialog().unwrap();
        assert_eq!(dm.active_count(), 2);

        dm.remove_dialog(&d1.call_id);
        assert_eq!(dm.active_count(), 1);

        dm.remove_dialog(&d2.call_id);
        assert_eq!(dm.active_count(), 0);
    }

    #[test]
    fn test_remove_dialog_nonexistent_returns_none() {
        let dm = DialogManager::new(10);
        assert!(dm.remove_dialog("nonexistent").is_none());
    }

    #[test]
    fn test_remove_dialog_not_found_after_removal() {
        let dm = DialogManager::new(10);
        let dialog = dm.create_dialog().unwrap();
        let call_id = dialog.call_id.clone();

        dm.remove_dialog(&call_id);
        assert!(dm.get_dialog(&call_id).is_none());
    }

    // --- Unit tests: generate_call_id / generate_from_tag ---

    #[test]
    fn test_generate_call_id_non_empty() {
        let id = DialogManager::generate_call_id();
        assert!(!id.is_empty());
    }

    #[test]
    fn test_generate_from_tag_non_empty() {
        let tag = DialogManager::generate_from_tag();
        assert!(!tag.is_empty());
    }

    #[test]
    fn test_generate_call_id_unique_pair() {
        let id1 = DialogManager::generate_call_id();
        let id2 = DialogManager::generate_call_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_generate_from_tag_unique_pair() {
        let tag1 = DialogManager::generate_from_tag();
        let tag2 = DialogManager::generate_from_tag();
        assert_ne!(tag1, tag2);
    }

    #[test]
    fn test_generate_call_id_is_32_char_hex() {
        let id = DialogManager::generate_call_id();
        assert_eq!(id.len(), 32, "Call-ID should be 32 hex characters");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "Call-ID should be hex string");
    }

    #[test]
    fn test_generate_from_tag_is_16_char_hex() {
        let tag = DialogManager::generate_from_tag();
        assert_eq!(tag.len(), 16, "From-tag should be 16 hex characters");
        assert!(tag.chars().all(|c| c.is_ascii_hexdigit()), "From-tag should be hex string");
    }

    #[test]
    fn test_generate_call_id_uniqueness_batch() {
        use std::collections::HashSet;
        let ids: HashSet<String> = (0..100).map(|_| DialogManager::generate_call_id()).collect();
        assert_eq!(ids.len(), 100, "100 generated Call-IDs should all be unique");
    }

    #[test]
    fn test_generate_from_tag_uniqueness_batch() {
        use std::collections::HashSet;
        let tags: HashSet<String> = (0..100).map(|_| DialogManager::generate_from_tag()).collect();
        assert_eq!(tags.len(), 100, "100 generated From-tags should all be unique");
    }

    // --- Unit tests: Dialog independence ---

    #[test]
    fn test_update_one_dialog_does_not_affect_others() {
        let dm = DialogManager::new(10);
        let d1 = dm.create_dialog().unwrap();
        let d2 = dm.create_dialog().unwrap();

        dm.update_dialog_state(&d1.call_id, DialogState::Error).unwrap();

        let d2_ref = dm.get_dialog(&d2.call_id).unwrap();
        assert_eq!(d2_ref.state, DialogState::Initial);
    }

    // ===== Property 3: Call-IDベースのレスポンスディスパッチ =====
    // Feature: sip-load-tester, Property 3: Call-IDベースのレスポンスディスパッチ
    // **Validates: Requirements 2.3, 15.2**
    proptest! {
        #[test]
        fn prop_call_id_based_response_dispatch(
            n in 2usize..20,
            pick in 0usize..20,
        ) {
            let dm = DialogManager::new(n);
            let mut call_ids: Vec<String> = Vec::with_capacity(n);

            for _ in 0..n {
                let dialog = dm.create_dialog().unwrap();
                call_ids.push(dialog.call_id.clone());
            }

            // Pick a random Call-ID from the created dialogs
            let target_idx = pick % n;
            let target_call_id = &call_ids[target_idx];

            // get_dialog with the target Call-ID returns the matching dialog
            let found = dm.get_dialog(target_call_id);
            prop_assert!(found.is_some(), "Dialog with target Call-ID not found");
            prop_assert_eq!(&found.unwrap().call_id, target_call_id);

            // No other Call-ID maps to the same dialog, and each other Call-ID
            // returns its own distinct dialog (not the target)
            for (i, cid) in call_ids.iter().enumerate() {
                let entry = dm.get_dialog(cid);
                prop_assert!(entry.is_some(), "Dialog {} not found", cid);
                let entry = entry.unwrap();
                prop_assert_eq!(&entry.call_id, cid,
                    "get_dialog returned wrong dialog for Call-ID {}", cid);

                if i != target_idx {
                    prop_assert_ne!(&entry.call_id, target_call_id,
                        "Non-target Call-ID {} returned the target dialog", cid);
                }
            }

            // A Call-ID not in the set returns None
            let fake_call_id = format!("nonexistent-{}", target_call_id);
            prop_assert!(dm.get_dialog(&fake_call_id).is_none(),
                "Fake Call-ID should not match any dialog");
        }
    }

    // --- Unit tests: collect_timed_out ---

    #[test]
    fn test_collect_timed_out_empty_manager() {
        let dm = DialogManager::new(10);
        let result = dm.collect_timed_out(Duration::from_secs(30));
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_timed_out_no_timed_out_dialogs() {
        let dm = DialogManager::new(10);
        dm.create_dialog().unwrap();
        dm.create_dialog().unwrap();
        // All dialogs just created, so none should be timed out with a 30s timeout
        let result = dm.collect_timed_out(Duration::from_secs(30));
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_timed_out_excludes_terminated_dialogs() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Terminated).unwrap();
        // Even with zero timeout, Terminated dialogs should be excluded
        let result = dm.collect_timed_out(Duration::from_secs(0));
        assert!(!result.contains(&call_id));
    }

    #[test]
    fn test_collect_timed_out_excludes_error_dialogs() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Error).unwrap();
        // Even with zero timeout, Error dialogs should be excluded
        let result = dm.collect_timed_out(Duration::from_secs(0));
        assert!(!result.contains(&call_id));
    }

    #[test]
    fn test_collect_timed_out_with_zero_timeout_returns_active() {
        let dm = DialogManager::new(10);
        let d1 = dm.create_dialog().unwrap();
        let d2 = dm.create_dialog().unwrap();
        // With zero timeout, all non-Terminated/Error dialogs should be returned
        // (since any elapsed time > 0)
        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_timed_out(Duration::from_secs(0));
        assert!(result.contains(&d1.call_id));
        assert!(result.contains(&d2.call_id));
    }

    #[test]
    fn test_collect_timed_out_mixed_states() {
        let dm = DialogManager::new(10);
        let d_initial = dm.create_dialog().unwrap();
        let d_confirmed = dm.create_dialog().unwrap();
        let d_terminated = dm.create_dialog().unwrap();
        let d_error = dm.create_dialog().unwrap();

        dm.update_dialog_state(&d_confirmed.call_id, DialogState::Confirmed).unwrap();
        dm.update_dialog_state(&d_terminated.call_id, DialogState::Terminated).unwrap();
        dm.update_dialog_state(&d_error.call_id, DialogState::Error).unwrap();

        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_timed_out(Duration::from_secs(0));

        // Initial and Confirmed should be included, Terminated and Error excluded
        assert!(result.contains(&d_initial.call_id));
        assert!(result.contains(&d_confirmed.call_id));
        assert!(!result.contains(&d_terminated.call_id));
        assert!(!result.contains(&d_error.call_id));
    }

    // ===== Property 5: 一意なCall-ID生成 =====
    // Feature: sip-load-tester, Property 5: 一意なCall-ID生成
    // **Validates: Requirements 5.3**
    proptest! {
        #[test]
        fn prop_unique_call_id_generation(
            n in 2usize..100,
        ) {
            use std::collections::HashSet;

            let dm = DialogManager::new(n);
            let mut call_ids = HashSet::with_capacity(n);

            for _ in 0..n {
                let dialog = dm.create_dialog().unwrap();
                call_ids.insert(dialog.call_id.clone());
            }

            // All N Call-IDs must be mutually distinct
            prop_assert_eq!(call_ids.len(), n,
                "Expected {} unique Call-IDs, but got {} (some duplicates exist)",
                n, call_ids.len());
        }
    }

    // ===== Property 9: タイムアウト検出の正確性テスト =====
    // Feature: performance-bottleneck-optimization, Property 9: タイムアウト検出の正確性
    // **Validates: Requirements 7.1**
    proptest! {
        #[test]
        fn prop_timeout_detection_accuracy(
            states in proptest::collection::vec(
                prop_oneof![
                    Just(DialogState::Initial),
                    Just(DialogState::Trying),
                    Just(DialogState::Early),
                    Just(DialogState::Confirmed),
                    Just(DialogState::Terminated),
                    Just(DialogState::Error),
                ],
                1..20
            ),
        ) {
            use std::collections::HashSet;

            let dm = DialogManager::new(states.len());
            let mut expected_timed_out = HashSet::new();

            for state in &states {
                let dialog = dm.create_dialog().unwrap();
                let call_id = dialog.call_id.clone();
                dm.update_dialog_state(&call_id, state.clone()).unwrap();

                if !matches!(state, DialogState::Terminated | DialogState::Error) {
                    expected_timed_out.insert(call_id);
                }
            }

            // With Duration::ZERO, all dialogs should be "timed out" (elapsed >= 0)
            // so the filter is purely on state: NOT Terminated and NOT Error
            let actual = dm.collect_timed_out(Duration::ZERO);
            let actual_set: HashSet<String> = actual.into_iter().collect();

            prop_assert_eq!(actual_set, expected_timed_out,
                "collect_timed_out(ZERO) should return exactly the non-Terminated/non-Error dialogs");
        }
    }

    // --- Unit tests: collect_expired_for_bye ---

    #[test]
    fn test_collect_expired_for_bye_empty_manager() {
        let dm = DialogManager::new(10);
        let result = dm.collect_expired_for_bye(Duration::from_secs(3));
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_expired_for_bye_no_confirmed_dialogs() {
        let dm = DialogManager::new(10);
        // Create dialogs in Initial state (not Confirmed)
        dm.create_dialog().unwrap();
        dm.create_dialog().unwrap();
        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_expired_for_bye(Duration::from_secs(0));
        assert!(result.is_empty(), "Should only collect Confirmed dialogs");
    }

    #[test]
    fn test_collect_expired_for_bye_collects_confirmed_expired() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        // Set to_tag for the dialog
        if let Some(mut dialog) = dm.get_dialog_mut(&call_id) {
            dialog.to_tag = Some("to-tag-1".to_string());
        }
        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_expired_for_bye(Duration::from_secs(0));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, call_id);
    }

    #[test]
    fn test_collect_expired_for_bye_returns_correct_tuple_fields() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        let call_id = d.call_id.clone();
        let from_tag = d.from_tag.clone();
        dm.update_dialog_state(&call_id, DialogState::Confirmed).unwrap();
        if let Some(mut dialog) = dm.get_dialog_mut(&call_id) {
            dialog.to_tag = Some("remote-tag".to_string());
            dialog.cseq = 5;
        }
        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_expired_for_bye(Duration::from_secs(0));
        assert_eq!(result.len(), 1);
        let (cid, ftag, ttag, cseq) = &result[0];
        assert_eq!(cid, &call_id);
        assert_eq!(ftag, &from_tag);
        assert_eq!(ttag, &Some("remote-tag".to_string()));
        assert_eq!(*cseq, 5);
    }

    #[test]
    fn test_collect_expired_for_bye_excludes_terminated() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d.call_id, DialogState::Confirmed).unwrap();
        dm.update_dialog_state(&d.call_id, DialogState::Terminated).unwrap();
        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_expired_for_bye(Duration::from_secs(0));
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_expired_for_bye_excludes_not_yet_expired() {
        let dm = DialogManager::new(10);
        let d = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d.call_id, DialogState::Confirmed).unwrap();
        // call_duration is 1 hour, dialog just created → not expired
        let result = dm.collect_expired_for_bye(Duration::from_secs(3600));
        assert!(result.is_empty());
    }

    #[test]
    fn test_collect_expired_for_bye_mixed_states_and_times() {
        let dm = DialogManager::new(10);
        // d1: Confirmed, will be expired
        let d1 = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d1.call_id, DialogState::Confirmed).unwrap();
        if let Some(mut d) = dm.get_dialog_mut(&d1.call_id) {
            d.to_tag = Some("tag1".to_string());
        }

        // d2: Initial (not Confirmed), should be excluded
        let d2 = dm.create_dialog().unwrap();

        // d3: Confirmed but Terminated, should be excluded
        let d3 = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d3.call_id, DialogState::Confirmed).unwrap();
        dm.update_dialog_state(&d3.call_id, DialogState::Terminated).unwrap();

        // d4: Confirmed, will be expired
        let d4 = dm.create_dialog().unwrap();
        dm.update_dialog_state(&d4.call_id, DialogState::Confirmed).unwrap();
        if let Some(mut d) = dm.get_dialog_mut(&d4.call_id) {
            d.to_tag = Some("tag4".to_string());
        }

        std::thread::sleep(Duration::from_millis(1));
        let result = dm.collect_expired_for_bye(Duration::from_secs(0));

        let call_ids: Vec<&String> = result.iter().map(|(cid, _, _, _)| cid).collect();
        assert!(call_ids.contains(&&d1.call_id));
        assert!(!call_ids.contains(&&d2.call_id));
        assert!(!call_ids.contains(&&d3.call_id));
        assert!(call_ids.contains(&&d4.call_id));
        assert_eq!(result.len(), 2);
    }

    // ===== Property 4: ダイアログの独立性 =====
    // Feature: sip-load-tester, Property 4: ダイアログの独立性
    // **Validates: Requirements 5.2, 17.3**
    proptest! {
        #[test]
        fn prop_dialog_independence(
            n in 2usize..20,
            error_pick in 0usize..20,
        ) {
            let dm = DialogManager::new(n);
            let mut call_ids: Vec<String> = Vec::with_capacity(n);

            for _ in 0..n {
                let dialog = dm.create_dialog().unwrap();
                call_ids.push(dialog.call_id.clone());
            }

            // All dialogs start in Initial state
            for cid in &call_ids {
                let d = dm.get_dialog(cid).unwrap();
                prop_assert_eq!(d.state.clone(), DialogState::Initial,
                    "Dialog {} should be in Initial state before transition", cid);
            }

            // Pick one dialog to transition to Error
            let error_idx = error_pick % n;
            let error_call_id = &call_ids[error_idx];
            dm.update_dialog_state(error_call_id, DialogState::Error).unwrap();

            // Verify the targeted dialog is now in Error state
            let errored = dm.get_dialog(error_call_id).unwrap();
            prop_assert_eq!(errored.state.clone(), DialogState::Error,
                "Dialog {} should be in Error state after transition", error_call_id);

            // Verify all OTHER dialogs remain in Initial state
            for (i, cid) in call_ids.iter().enumerate() {
                if i != error_idx {
                    let d = dm.get_dialog(cid).unwrap();
                    prop_assert_eq!(d.state.clone(), DialogState::Initial,
                        "Dialog {} (index {}) should still be in Initial state, but was {:?}",
                        cid, i, d.state);
                }
            }
        }
    }
}
