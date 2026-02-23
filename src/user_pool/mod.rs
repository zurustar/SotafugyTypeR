use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::SipLoadTestError;

/// users.jsonのルート構造
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsersFile {
    pub users: Vec<UserEntry>,
}

/// 個別ユーザエントリ
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserEntry {
    pub username: String,
    pub domain: String,
    pub password: String,
}

/// ユーザ選択戦略
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionStrategy {
    RoundRobin,
    Random,
}

/// ユーザプール（共有リソース）
pub struct UserPool {
    users: Vec<UserEntry>,
    index: AtomicUsize,
}

impl std::fmt::Debug for UserPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserPool")
            .field("users", &self.users)
            .field("index", &self.index.load(Ordering::Relaxed))
            .finish()
    }
}

impl UserPool {
    /// users.jsonファイルからロード
    pub fn load_from_file(path: &Path) -> Result<Self, SipLoadTestError> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            SipLoadTestError::UserPoolError(format!("Failed to read file: {}", e))
        })?;
        let users_file: UsersFile = serde_json::from_str(&content).map_err(|e| {
            SipLoadTestError::UserPoolError(format!("Failed to parse JSON: {}", e))
        })?;
        Self::from_users_file(users_file)
    }

    /// UsersFileから構築
    pub fn from_users_file(users_file: UsersFile) -> Result<Self, SipLoadTestError> {
        if users_file.users.is_empty() {
            return Err(SipLoadTestError::EmptyUserPool);
        }
        Ok(Self {
            users: users_file.users,
            index: AtomicUsize::new(0),
        })
    }

    /// ラウンドロビンで次のユーザを取得
    pub fn next_user(&self) -> &UserEntry {
        let idx = self.index.fetch_add(1, Ordering::Relaxed) % self.users.len();
        &self.users[idx]
    }

    /// ランダムにユーザを取得
    pub fn random_user(&self) -> &UserEntry {
        use rand::Rng;
        let idx = rand::thread_rng().gen_range(0..self.users.len());
        &self.users[idx]
    }

    /// 指定戦略でユーザを取得
    pub fn select_user(&self, strategy: SelectionStrategy) -> &UserEntry {
        match strategy {
            SelectionStrategy::RoundRobin => self.next_user(),
            SelectionStrategy::Random => self.random_user(),
        }
    }

    /// usernameでユーザを検索（プロキシ認証検証用）
    pub fn find_by_username(&self, username: &str) -> Option<&UserEntry> {
        self.users.iter().find(|u| u.username == username)
    }

    /// ユーザ数を返す
    pub fn len(&self) -> usize {
        self.users.len()
    }
}

/// ユーザファイル生成
pub struct UserGenerator;

impl UserGenerator {
    /// 指定パラメータでユーザリストを生成
    ///
    /// - Username format: `{prefix}{index:04}` (zero-padded to 4 digits)
    /// - Password: replace `{index}` in password_pattern with the zero-padded index
    pub fn generate(
        prefix: &str,
        start: u32,
        count: u32,
        domain: &str,
        password_pattern: &str,
    ) -> UsersFile {
        let users = (0..count)
            .map(|i| {
                let index = start + i;
                let padded = format!("{:04}", index);
                UserEntry {
                    username: format!("{}{}", prefix, padded),
                    domain: domain.to_string(),
                    password: password_pattern.replace("{index}", &padded),
                }
            })
            .collect();
        UsersFile { users }
    }

    /// ファイルに書き出し（新規作成）
    pub fn write_to_file(users_file: &UsersFile, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(users_file)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// 既存ファイルに追記（既存ユーザを保持）
    pub fn append_to_file(new_users: &UsersFile, path: &Path) -> anyhow::Result<()> {
        let mut merged = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str::<UsersFile>(&content)?
        } else {
            UsersFile { users: vec![] }
        };
        merged.users.extend(new_users.users.iter().cloned());
        let json = serde_json::to_string_pretty(&merged)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use proptest::prelude::*;

    fn make_users(n: usize) -> Vec<UserEntry> {
        (0..n)
            .map(|i| UserEntry {
                username: format!("user{:04}", i + 1),
                domain: "example.com".to_string(),
                password: format!("pass{:04}", i + 1),
            })
            .collect()
    }

    fn make_pool(n: usize) -> UserPool {
        let uf = UsersFile { users: make_users(n) };
        UserPool::from_users_file(uf).unwrap()
    }

    // --- Construction tests ---

    #[test]
    fn from_users_file_creates_pool() {
        let pool = make_pool(3);
        assert_eq!(pool.len(), 3);
    }

    #[test]
    fn from_users_file_empty_returns_error() {
        let uf = UsersFile { users: vec![] };
        let result = UserPool::from_users_file(uf);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SipLoadTestError::EmptyUserPool
        ));
    }

    #[test]
    fn from_users_file_single_user() {
        let pool = make_pool(1);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool.next_user().username, "user0001");
    }

    // --- next_user round-robin tests ---

    #[test]
    fn next_user_round_robin_cycles() {
        let pool = make_pool(3);
        // First cycle
        assert_eq!(pool.next_user().username, "user0001");
        assert_eq!(pool.next_user().username, "user0002");
        assert_eq!(pool.next_user().username, "user0003");
        // Second cycle wraps around
        assert_eq!(pool.next_user().username, "user0001");
        assert_eq!(pool.next_user().username, "user0002");
    }

    #[test]
    fn next_user_single_user_always_returns_same() {
        let pool = make_pool(1);
        for _ in 0..5 {
            assert_eq!(pool.next_user().username, "user0001");
        }
    }

    // --- random_user tests ---

    #[test]
    fn random_user_returns_valid_user() {
        let pool = make_pool(5);
        for _ in 0..20 {
            let user = pool.random_user();
            assert!(user.username.starts_with("user"));
            assert_eq!(user.domain, "example.com");
        }
    }

    // --- select_user tests ---

    #[test]
    fn select_user_round_robin_delegates_to_next_user() {
        let pool = make_pool(2);
        let u1 = pool.select_user(SelectionStrategy::RoundRobin);
        assert_eq!(u1.username, "user0001");
        let u2 = pool.select_user(SelectionStrategy::RoundRobin);
        assert_eq!(u2.username, "user0002");
    }

    #[test]
    fn select_user_random_returns_valid_user() {
        let pool = make_pool(3);
        let user = pool.select_user(SelectionStrategy::Random);
        assert!(pool.find_by_username(&user.username).is_some());
    }

    // --- find_by_username tests ---

    #[test]
    fn find_by_username_existing() {
        let pool = make_pool(3);
        let found = pool.find_by_username("user0002");
        assert!(found.is_some());
        let entry = found.unwrap();
        assert_eq!(entry.username, "user0002");
        assert_eq!(entry.password, "pass0002");
    }

    #[test]
    fn find_by_username_not_found() {
        let pool = make_pool(3);
        assert!(pool.find_by_username("nonexistent").is_none());
    }

    // --- load_from_file tests ---

    #[test]
    fn load_from_file_valid_json() {
        let json = r#"{
            "users": [
                { "username": "alice", "domain": "sip.example.com", "password": "secret1" },
                { "username": "bob", "domain": "sip.example.com", "password": "secret2" }
            ]
        }"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(json.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let pool = UserPool::load_from_file(tmp.path()).unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.find_by_username("alice").unwrap().password, "secret1");
    }

    #[test]
    fn load_from_file_empty_users_returns_error() {
        let json = r#"{ "users": [] }"#;
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(json.as_bytes()).unwrap();
        tmp.flush().unwrap();

        let result = UserPool::load_from_file(tmp.path());
        assert!(matches!(result.unwrap_err(), SipLoadTestError::EmptyUserPool));
    }

    #[test]
    fn load_from_file_nonexistent_returns_error() {
        let result = UserPool::load_from_file(Path::new("/nonexistent/users.json"));
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::UserPoolError(msg) => {
                assert!(msg.contains("Failed to read file"));
            }
            other => panic!("Expected UserPoolError, got {:?}", other),
        }
    }

    #[test]
    fn load_from_file_invalid_json_returns_error() {
        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(b"not json").unwrap();
        tmp.flush().unwrap();

        let result = UserPool::load_from_file(tmp.path());
        assert!(result.is_err());
        match result.unwrap_err() {
            SipLoadTestError::UserPoolError(msg) => {
                assert!(msg.contains("Failed to parse JSON"));
            }
            other => panic!("Expected UserPoolError, got {:?}", other),
        }
    }

    // --- len tests ---

    #[test]
    fn len_returns_correct_count() {
        assert_eq!(make_pool(1).len(), 1);
        assert_eq!(make_pool(5).len(), 5);
        assert_eq!(make_pool(100).len(), 100);
    }

    // --- Serde round-trip for data models ---

    #[test]
    fn user_entry_serde_roundtrip() {
        let entry = UserEntry {
            username: "test".to_string(),
            domain: "example.com".to_string(),
            password: "pass".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: UserEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deserialized);
    }

    #[test]
    fn users_file_serde_roundtrip() {
        let uf = UsersFile { users: make_users(3) };
        let json = serde_json::to_string(&uf).unwrap();
        let deserialized: UsersFile = serde_json::from_str(&json).unwrap();
        assert_eq!(uf, deserialized);
    }

    // --- UserGenerator::generate tests ---

    #[test]
    fn generate_creates_correct_count() {
        let result = UserGenerator::generate("user", 1, 3, "example.com", "pass{index}");
        assert_eq!(result.users.len(), 3);
    }

    #[test]
    fn generate_zero_count_returns_empty() {
        let result = UserGenerator::generate("user", 1, 0, "example.com", "pass{index}");
        assert_eq!(result.users.len(), 0);
    }

    #[test]
    fn generate_username_format() {
        let result = UserGenerator::generate("user", 1, 3, "example.com", "pass{index}");
        assert_eq!(result.users[0].username, "user0001");
        assert_eq!(result.users[1].username, "user0002");
        assert_eq!(result.users[2].username, "user0003");
    }

    #[test]
    fn generate_password_pattern_replacement() {
        let result = UserGenerator::generate("user", 1, 3, "example.com", "pass{index}");
        assert_eq!(result.users[0].password, "pass0001");
        assert_eq!(result.users[1].password, "pass0002");
        assert_eq!(result.users[2].password, "pass0003");
    }

    #[test]
    fn generate_domain_applied_to_all() {
        let result = UserGenerator::generate("sip", 1, 3, "sip.test.org", "pw{index}");
        for user in &result.users {
            assert_eq!(user.domain, "sip.test.org");
        }
    }

    #[test]
    fn generate_custom_start_index() {
        let result = UserGenerator::generate("u", 10, 2, "d.com", "p{index}");
        assert_eq!(result.users[0].username, "u0010");
        assert_eq!(result.users[0].password, "p0010");
        assert_eq!(result.users[1].username, "u0011");
        assert_eq!(result.users[1].password, "p0011");
    }

    #[test]
    fn generate_password_without_index_placeholder() {
        let result = UserGenerator::generate("user", 1, 2, "example.com", "fixedpass");
        assert_eq!(result.users[0].password, "fixedpass");
        assert_eq!(result.users[1].password, "fixedpass");
    }

    // --- UserGenerator::write_to_file tests ---

    #[test]
    fn write_to_file_creates_valid_json() {
        let users_file = UserGenerator::generate("user", 1, 2, "example.com", "pass{index}");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        // Close the temp file so write_to_file can create it fresh
        drop(tmp);

        UserGenerator::write_to_file(&users_file, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded, users_file);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_to_file_pretty_json() {
        let users_file = UserGenerator::generate("user", 1, 1, "example.com", "pass{index}");
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        drop(tmp);

        UserGenerator::write_to_file(&users_file, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // Pretty JSON should contain newlines
        assert!(content.contains('\n'));

        std::fs::remove_file(&path).ok();
    }

    // --- UserGenerator::append_to_file tests ---

    #[test]
    fn append_to_file_creates_file_if_not_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_users.json");

        let new_users = UserGenerator::generate("user", 1, 2, "example.com", "pass{index}");
        UserGenerator::append_to_file(&new_users, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.users.len(), 2);
        assert_eq!(loaded, new_users);
    }

    #[test]
    fn append_to_file_preserves_existing_users() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");

        // Write initial users
        let initial = UserGenerator::generate("user", 1, 2, "example.com", "pass{index}");
        UserGenerator::write_to_file(&initial, &path).unwrap();

        // Append new users
        let new_users = UserGenerator::generate("user", 3, 2, "example.com", "pass{index}");
        UserGenerator::append_to_file(&new_users, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: UsersFile = serde_json::from_str(&content).unwrap();
        assert_eq!(loaded.users.len(), 4);
        // Existing users preserved
        assert_eq!(loaded.users[0].username, "user0001");
        assert_eq!(loaded.users[1].username, "user0002");
        // New users appended
        assert_eq!(loaded.users[2].username, "user0003");
        assert_eq!(loaded.users[3].username, "user0004");
    }

    #[test]
    fn append_to_file_existing_users_not_modified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.json");

        let initial = UserGenerator::generate("admin", 1, 1, "corp.com", "secret{index}");
        UserGenerator::write_to_file(&initial, &path).unwrap();

        let new_users = UserGenerator::generate("user", 1, 1, "example.com", "pass{index}");
        UserGenerator::append_to_file(&new_users, &path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let loaded: UsersFile = serde_json::from_str(&content).unwrap();
        // Original user unchanged
        assert_eq!(loaded.users[0].username, "admin0001");
        assert_eq!(loaded.users[0].domain, "corp.com");
        assert_eq!(loaded.users[0].password, "secret0001");
    }

    // ===== Property 21: UserPoolのラウンドトリップ =====
    // Feature: sip-load-tester, Property 21: UserPoolのラウンドトリップ
    // **Validates: Requirements 14.7（UserPool関連）**

    fn arb_user_entry() -> impl Strategy<Value = UserEntry> {
        (
            "[a-z][a-z0-9]{0,15}",   // username: starts with letter, alphanumeric
            "[a-z]{1,10}\\.[a-z]{2,5}", // domain: e.g. "example.com"
            ".{1,20}",                // password: any non-empty string
        )
            .prop_map(|(username, domain, password)| UserEntry {
                username,
                domain,
                password,
            })
    }

    fn arb_users_file() -> impl Strategy<Value = UsersFile> {
        prop::collection::vec(arb_user_entry(), 1..20)
            .prop_map(|users| UsersFile { users })
    }

    // ===== Property 22: ユーザ生成の正確性と一意性 =====
    // Feature: sip-load-tester, Property 22: ユーザ生成の正確性と一意性
    // **Validates: Requirements 20.5（UserPool関連）**

    fn arb_prefix() -> impl Strategy<Value = String> {
        "[a-z]{1,8}"
    }

    fn arb_domain() -> impl Strategy<Value = String> {
        "[a-z]{1,8}\\.[a-z]{2,4}"
    }

    fn arb_password_pattern() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("pass{index}".to_string()),
            Just("secret{index}".to_string()),
            Just("fixedpass".to_string()),
            Just("pw-{index}-end".to_string()),
        ]
    }

    proptest! {
        #[test]
        fn prop_users_file_roundtrip(users_file in arb_users_file()) {
            let json = serde_json::to_string(&users_file).unwrap();
            let deserialized: UsersFile = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(users_file, deserialized);
        }

        #[test]
        fn prop_user_generator_correctness_and_uniqueness(
            prefix in arb_prefix(),
            start in 1u32..100,
            count in 1u32..50,
            domain in arb_domain(),
            password_pattern in arb_password_pattern(),
        ) {
            let result = UserGenerator::generate(&prefix, start, count, &domain, &password_pattern);

            // (a) ユーザ数がcountと一致する
            prop_assert_eq!(result.users.len(), count as usize,
                "User count mismatch: expected {}, got {}", count, result.users.len());

            // (b) 全ユーザ名が互いに一意である
            let mut usernames: Vec<&str> = result.users.iter().map(|u| u.username.as_str()).collect();
            let total = usernames.len();
            usernames.sort();
            usernames.dedup();
            prop_assert_eq!(usernames.len(), total,
                "Duplicate usernames found");

            // (c) 各ユーザ名がprefix+ゼロ埋めインデックスの形式に従う
            for (i, user) in result.users.iter().enumerate() {
                let expected_username = format!("{}{:04}", prefix, start + i as u32);
                prop_assert_eq!(&user.username, &expected_username,
                    "Username format mismatch at index {}: expected {}, got {}",
                    i, expected_username, user.username);
            }

            // (d) 各ドメインが指定domainと一致する
            for (i, user) in result.users.iter().enumerate() {
                prop_assert_eq!(&user.domain, &domain,
                    "Domain mismatch at index {}: expected {}, got {}",
                    i, domain, user.domain);
            }
        }

        // ===== Property 23: appendモードでの既存ユーザ保持 =====
        // Feature: sip-load-tester, Property 23: appendモードでの既存ユーザ保持
        // **Validates: Requirements 20.5（UserPool関連）**
        #[test]
        fn prop_append_preserves_existing_users(
            initial in arb_users_file(),
            prefix in arb_prefix(),
            start in 1u32..100,
            count in 1u32..20,
            domain in arb_domain(),
            password_pattern in arb_password_pattern(),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("users.json");

            // Write initial users to file
            UserGenerator::write_to_file(&initial, &path).unwrap();

            // Generate new users and append
            let new_users = UserGenerator::generate(&prefix, start, count, &domain, &password_pattern);
            UserGenerator::append_to_file(&new_users, &path).unwrap();

            // Read back the resulting file
            let content = std::fs::read_to_string(&path).unwrap();
            let result: UsersFile = serde_json::from_str(&content).unwrap();

            // Total user count = original + new
            let expected_total = initial.users.len() + new_users.users.len();
            prop_assert_eq!(result.users.len(), expected_total,
                "Total user count mismatch: expected {}, got {}", expected_total, result.users.len());

            // All original users are preserved in order at the beginning
            for (i, original_user) in initial.users.iter().enumerate() {
                prop_assert_eq!(&result.users[i], original_user,
                    "Original user at index {} was modified", i);
            }

            // New users are appended after the original users
            let offset = initial.users.len();
            for (i, new_user) in new_users.users.iter().enumerate() {
                prop_assert_eq!(&result.users[offset + i], new_user,
                    "New user at index {} does not match", i);
            }
        }

        // ===== Property 24: ラウンドロビン選択の均等性 =====
        // Feature: sip-load-tester, Property 24: ラウンドロビン選択の均等性
        // **Validates: Requirements 2.1, 2.2（UserPool関連）**
        #[test]
        fn prop_round_robin_even_distribution(
            n in 1usize..20,
            m in 1usize..10,
        ) {
            let pool = make_pool(n);
            let total_calls = n * m;

            // Count how many times each user is selected
            let mut counts = std::collections::HashMap::new();
            for _ in 0..total_calls {
                let user = pool.next_user();
                *counts.entry(user.username.clone()).or_insert(0usize) += 1;
            }

            // Each of the N users should be selected exactly M times
            prop_assert_eq!(counts.len(), n,
                "Expected {} distinct users, got {}", n, counts.len());

            for (username, count) in &counts {
                prop_assert_eq!(*count, m,
                    "User {} was selected {} times, expected {}", username, count, m);
            }
        }
    }
}
