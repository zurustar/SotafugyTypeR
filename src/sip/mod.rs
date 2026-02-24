pub mod parser;
pub mod formatter;
pub mod message;

use std::hash::{Hash, Hasher};
#[allow(deprecated)]
use std::hash::SipHasher;

/// RFC 3261 Section 16.11に準拠したbranch値生成関数。
/// Call-ID、CSeq番号、メソッドのハッシュから`z9hG4bK`プレフィックス付きのbranch値を生成する。
/// 同一パラメータに対しては常に同一のbranch値を返す（冪等性）。
pub fn generate_branch(call_id: &str, cseq: u32, method: &str) -> String {
    use std::fmt::Write;
    #[allow(deprecated)]
    let mut hasher = SipHasher::new();
    call_id.hash(&mut hasher);
    cseq.hash(&mut hasher);
    method.hash(&mut hasher);
    let hash_value = hasher.finish();
    // "z9hG4bK" (7) + 16 hex chars = 23 chars
    let mut buf = String::with_capacity(23);
    buf.push_str("z9hG4bK");
    write!(buf, "{:016x}", hash_value).unwrap();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_branch_starts_with_magic_cookie() {
        let branch = generate_branch("call-id-123", 1, "INVITE");
        assert!(
            branch.starts_with("z9hG4bK"),
            "branch値は z9hG4bK プレフィックスで始まるべきです: {}",
            branch
        );
    }

    #[test]
    fn test_generate_branch_idempotent() {
        let branch1 = generate_branch("call-id-abc", 1, "INVITE");
        let branch2 = generate_branch("call-id-abc", 1, "INVITE");
        assert_eq!(
            branch1, branch2,
            "同一パラメータからは同一のbranch値が生成されるべきです"
        );
    }

    #[test]
    fn test_generate_branch_different_cseq() {
        let branch1 = generate_branch("call-id-abc", 1, "INVITE");
        let branch2 = generate_branch("call-id-abc", 2, "BYE");
        assert_ne!(
            branch1, branch2,
            "異なるCSeq番号/メソッドからは異なるbranch値が生成されるべきです"
        );
    }

    #[test]
    fn test_generate_branch_different_methods_same_cseq() {
        let branch1 = generate_branch("call-id-abc", 1, "INVITE");
        let branch2 = generate_branch("call-id-abc", 1, "ACK");
        assert_ne!(
            branch1, branch2,
            "異なるメソッドからは異なるbranch値が生成されるべきです"
        );
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// **Validates: Requirements 3.6**
        /// branch値の冪等性テスト: 同一パラメータで複数回呼び出し、同一結果を検証
        #[test]
        fn prop_generate_branch_idempotent(
            call_id in "[a-zA-Z0-9\\-]{1,64}",
            cseq in 1u32..=u32::MAX,
            method in prop_oneof!["INVITE", "REGISTER", "ACK", "BYE", "OPTIONS", "CANCEL"]
        ) {
            let branch1 = generate_branch(&call_id, cseq, &method);
            let branch2 = generate_branch(&call_id, cseq, &method);
            prop_assert_eq!(branch1, branch2, "同一パラメータからは同一のbranch値が生成されるべきです");
        }

        /// **Validates: Requirements 3.5**
        /// branch値のマジッククッキーテスト: `z9hG4bK`プレフィックスで始まることを検証
        #[test]
        fn prop_generate_branch_magic_cookie(
            call_id in "[a-zA-Z0-9\\-]{1,64}",
            cseq in 1u32..=u32::MAX,
            method in prop_oneof!["INVITE", "REGISTER", "ACK", "BYE", "OPTIONS", "CANCEL"]
        ) {
            let branch = generate_branch(&call_id, cseq, &method);
            prop_assert!(
                branch.starts_with("z9hG4bK"),
                "branch値は z9hG4bK プレフィックスで始まるべきです: {}",
                branch
            );
        }

        /// **Validates: Requirements 2.5, 2.6**
        /// branch値のユニーク性テスト: 異なるトランザクションパラメータからは異なるbranch値が生成されること
        #[test]
        fn prop_generate_branch_unique_for_different_transactions(
            call_id in "[a-zA-Z0-9\\-]{1,64}",
            cseq1 in 1u32..=1000000u32,
            cseq2 in 1u32..=1000000u32,
            method1 in prop_oneof!["INVITE", "REGISTER", "ACK", "BYE"],
            method2 in prop_oneof!["INVITE", "REGISTER", "ACK", "BYE"],
        ) {
            prop_assume!(cseq1 != cseq2 || method1 != method2);
            let branch1 = generate_branch(&call_id, cseq1, &method1);
            let branch2 = generate_branch(&call_id, cseq2, &method2);
            prop_assert_ne!(
                branch1, branch2,
                "異なるトランザクションパラメータからは異なるbranch値が生成されるべきです"
            );
        }
    }
}
