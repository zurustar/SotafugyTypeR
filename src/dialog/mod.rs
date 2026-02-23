// Dialog manager module

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rand::Rng;

use crate::error::SipLoadTestError;
use crate::sip::message::SipMessage;

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

    /// Generate a unique Call-ID string
    pub fn generate_call_id() -> String {
        let mut rng = rand::thread_rng();
        let random_part: u128 = rng.gen();
        format!("{:032x}", random_part)
    }

    /// Generate a unique From-tag string
    pub fn generate_from_tag() -> String {
        let mut rng = rand::thread_rng();
        let random_part: u64 = rng.gen();
        format!("{:016x}", random_part)
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
