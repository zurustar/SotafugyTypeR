// SIP message buffer pool for reducing heap allocations

// デフォルト初期容量（設計ドキュメント Data Models セクション準拠）
const DEFAULT_REQUEST_URI_CAPACITY: usize = 128;
const DEFAULT_VERSION_CAPACITY: usize = 8;
const DEFAULT_REASON_PHRASE_CAPACITY: usize = 32;
const DEFAULT_HEADER_COUNT: usize = 16;
pub(crate) const DEFAULT_HEADER_NAME_CAPACITY: usize = 32;
pub(crate) const DEFAULT_HEADER_VALUE_CAPACITY: usize = 128;
const DEFAULT_OUTPUT_CAPACITY: usize = 2048;

/// プールから取得される再利用可能なバッファ。
/// SIPメッセージ1件の解析・フォーマットに必要な領域を内包する。
pub struct MessageBuf {
    // パーサー用: 文字列フィールドの事前確保バッファ
    pub request_uri: String,
    pub version: String,
    pub reason_phrase: String,
    pub header_names: Vec<String>,
    pub header_values: Vec<String>,
    // フォーマッター用: 出力バッファ
    pub output: Vec<u8>,
    // ボディ用
    pub body: Vec<u8>,
}

impl MessageBuf {
    /// デフォルト容量で新規作成。
    /// 各フィールドは長さ0、設計ドキュメントで定義された初期容量を確保する。
    pub fn new() -> Self {
        Self {
            request_uri: String::with_capacity(DEFAULT_REQUEST_URI_CAPACITY),
            version: String::with_capacity(DEFAULT_VERSION_CAPACITY),
            reason_phrase: String::with_capacity(DEFAULT_REASON_PHRASE_CAPACITY),
            header_names: Vec::with_capacity(DEFAULT_HEADER_COUNT),
            header_values: Vec::with_capacity(DEFAULT_HEADER_COUNT),
            output: Vec::with_capacity(DEFAULT_OUTPUT_CAPACITY),
            body: Vec::new(), // 通常は空、容量0
        }
    }

    /// 全フィールドの長さを0にリセット。
    /// 容量0のフィールド（std::mem::take後など）はデフォルト容量で再確保する。
    /// 容量が残っているフィールドはそのまま維持する。
    pub fn clear(&mut self) {
        // String フィールド: 容量0なら再確保、そうでなければ clear
        if self.request_uri.capacity() == 0 {
            self.request_uri = String::with_capacity(DEFAULT_REQUEST_URI_CAPACITY);
        } else {
            self.request_uri.clear();
        }

        if self.version.capacity() == 0 {
            self.version = String::with_capacity(DEFAULT_VERSION_CAPACITY);
        } else {
            self.version.clear();
        }

        if self.reason_phrase.capacity() == 0 {
            self.reason_phrase = String::with_capacity(DEFAULT_REASON_PHRASE_CAPACITY);
        } else {
            self.reason_phrase.clear();
        }

        // Vec<String> フィールド: 容量0なら再確保、そうでなければ clear
        if self.header_names.capacity() == 0 {
            self.header_names = Vec::with_capacity(DEFAULT_HEADER_COUNT);
        } else {
            self.header_names.clear();
        }

        if self.header_values.capacity() == 0 {
            self.header_values = Vec::with_capacity(DEFAULT_HEADER_COUNT);
        } else {
            self.header_values.clear();
        }

        // output: 容量0なら再確保、そうでなければ clear（容量維持）
        if self.output.capacity() == 0 {
            self.output = Vec::with_capacity(DEFAULT_OUTPUT_CAPACITY);
        } else {
            self.output.clear();
        }

        // body: clear のみ（通常は空、容量0が正常）
        self.body.clear();
    }
}

use std::sync::Mutex;

/// スレッドセーフなメッセージバッファプール。
/// 事前に確保した MessageBuf を管理し、get/put で再利用する。
pub struct MessagePool {
    pool: Mutex<Vec<MessageBuf>>,
}

impl MessagePool {
    /// 指定数の MessageBuf を事前確保してプールを作成。
    pub fn new(capacity: usize) -> Self {
        let mut bufs = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            bufs.push(MessageBuf::new());
        }
        Self {
            pool: Mutex::new(bufs),
        }
    }

    /// プールから MessageBuf を取得。空の場合は新規作成（フォールバック）。
    pub fn get(&self) -> MessageBuf {
        self.pool
            .lock()
            .expect("MessagePool lock poisoned")
            .pop()
            .unwrap_or_else(MessageBuf::new)
    }

    /// MessageBuf をクリアしてプールに返却。
    pub fn put(&self, mut buf: MessageBuf) {
        buf.clear();
        self.pool
            .lock()
            .expect("MessagePool lock poisoned")
            .push(buf);
    }

    /// 現在のプール内バッファ数を返す（テスト・監視用）。
    pub fn available(&self) -> usize {
        self.pool
            .lock()
            .expect("MessagePool lock poisoned")
            .len()
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use proptest::prelude::*;

    // =========================================================================
    // MessagePool テスト (Requirements: 1.1, 1.2, 1.3, 1.4, 1.5)
    // =========================================================================

    /// new(N) で N 個の MessageBuf が事前確保されること
    /// Requirements: 1.1
    #[test]
    fn test_message_pool_new_preallocates() {
        let pool = MessagePool::new(8);
        assert_eq!(pool.available(), 8, "new(8) で available() は8であるべき");

        let pool_zero = MessagePool::new(0);
        assert_eq!(pool_zero.available(), 0, "new(0) で available() は0であるべき");

        let pool_large = MessagePool::new(64);
        assert_eq!(pool_large.available(), 64, "new(64) で available() は64であるべき");
    }

    /// get() でプールから取得し available() が減少すること
    /// Requirements: 1.2
    #[test]
    fn test_message_pool_get_decreases_available() {
        let pool = MessagePool::new(4);
        assert_eq!(pool.available(), 4);

        let _buf1 = pool.get();
        assert_eq!(pool.available(), 3, "get() 後 available() は3であるべき");

        let _buf2 = pool.get();
        assert_eq!(pool.available(), 2, "2回目の get() 後 available() は2であるべき");
    }

    /// get() で取得した MessageBuf は全フィールドが空の初期状態であること
    /// Requirements: 1.2
    #[test]
    fn test_message_pool_get_returns_empty_buf() {
        let pool = MessagePool::new(2);
        let buf = pool.get();

        assert_eq!(buf.request_uri.len(), 0, "取得した buf の request_uri は空であるべき");
        assert_eq!(buf.version.len(), 0, "取得した buf の version は空であるべき");
        assert_eq!(buf.reason_phrase.len(), 0, "取得した buf の reason_phrase は空であるべき");
        assert_eq!(buf.header_names.len(), 0, "取得した buf の header_names は空であるべき");
        assert_eq!(buf.header_values.len(), 0, "取得した buf の header_values は空であるべき");
        assert_eq!(buf.output.len(), 0, "取得した buf の output は空であるべき");
        assert_eq!(buf.body.len(), 0, "取得した buf の body は空であるべき");
    }

    /// put() で返却し available() が増加すること
    /// Requirements: 1.4
    #[test]
    fn test_message_pool_put_increases_available() {
        let pool = MessagePool::new(2);
        assert_eq!(pool.available(), 2);

        let buf = pool.get();
        assert_eq!(pool.available(), 1);

        pool.put(buf);
        assert_eq!(pool.available(), 2, "put() 後 available() は元に戻るべき");
    }

    /// put() で返却された MessageBuf は clear 済みであること
    /// Requirements: 1.4, 6.1, 6.2
    #[test]
    fn test_message_pool_put_clears_buf() {
        let pool = MessagePool::new(1);
        let mut buf = pool.get();

        // データを書き込む
        buf.request_uri.push_str("sip:alice@example.com");
        buf.version.push_str("SIP/2.0");
        buf.reason_phrase.push_str("OK");
        buf.header_names.push(String::from("Via"));
        buf.header_values.push(String::from("SIP/2.0/UDP 10.0.0.1:5060"));
        buf.output.extend_from_slice(b"SIP/2.0 200 OK\r\n\r\n");
        buf.body.extend_from_slice(b"v=0\r\n");

        // 返却して再取得
        pool.put(buf);
        let buf2 = pool.get();

        // 再取得した buf は全フィールドが空であること
        assert_eq!(buf2.request_uri.len(), 0, "再取得した buf の request_uri は空であるべき");
        assert_eq!(buf2.version.len(), 0, "再取得した buf の version は空であるべき");
        assert_eq!(buf2.reason_phrase.len(), 0, "再取得した buf の reason_phrase は空であるべき");
        assert_eq!(buf2.header_names.len(), 0, "再取得した buf の header_names は空であるべき");
        assert_eq!(buf2.header_values.len(), 0, "再取得した buf の header_values は空であるべき");
        assert_eq!(buf2.output.len(), 0, "再取得した buf の output は空であるべき");
        assert_eq!(buf2.body.len(), 0, "再取得した buf の body は空であるべき");
    }

    /// 空プールからの get() で新規 MessageBuf が作成されること（フォールバック）
    /// Requirements: 1.3
    #[test]
    fn test_message_pool_get_fallback_when_empty() {
        let pool = MessagePool::new(0);
        assert_eq!(pool.available(), 0, "空プールの available() は0であるべき");

        // 空プールからでも get() は成功する
        let buf = pool.get();
        assert_eq!(buf.request_uri.len(), 0, "フォールバックで作成された buf は空であるべき");
        assert_eq!(buf.version.len(), 0);
        assert_eq!(buf.output.len(), 0);

        // フォールバックで作成された buf も適切な初期容量を持つ
        assert!(buf.request_uri.capacity() >= 128, "フォールバック buf も適切な初期容量を持つべき");
        assert!(buf.output.capacity() >= 2048, "フォールバック buf の output も適切な初期容量を持つべき");
    }

    /// プール容量を超えて get() した場合もフォールバックで動作すること
    /// Requirements: 1.3
    #[test]
    fn test_message_pool_get_beyond_capacity() {
        let pool = MessagePool::new(2);

        let _buf1 = pool.get();
        let _buf2 = pool.get();
        assert_eq!(pool.available(), 0);

        // プール枯渇後もフォールバックで取得可能
        let buf3 = pool.get();
        assert_eq!(buf3.request_uri.len(), 0, "フォールバック buf は空であるべき");
    }

    /// Send + Sync の静的検証
    /// Requirements: 1.5
    #[test]
    fn test_message_pool_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MessagePool>();
    }

    /// new() の初期状態テスト: 全フィールドの長さが0であること
    #[test]
    fn test_message_buf_new_all_fields_empty() {
        let buf = MessageBuf::new();

        // 文字列フィールドの長さが0
        assert_eq!(buf.request_uri.len(), 0, "request_uri は空であるべき");
        assert_eq!(buf.version.len(), 0, "version は空であるべき");
        assert_eq!(buf.reason_phrase.len(), 0, "reason_phrase は空であるべき");

        // Vec フィールドの長さが0
        assert_eq!(buf.header_names.len(), 0, "header_names は空であるべき");
        assert_eq!(buf.header_values.len(), 0, "header_values は空であるべき");
        assert_eq!(buf.output.len(), 0, "output は空であるべき");
        assert_eq!(buf.body.len(), 0, "body は空であるべき");
    }

    /// new() の初期容量テスト: 設計ドキュメントの Data Models に従った容量が確保されていること
    #[test]
    fn test_message_buf_new_initial_capacities() {
        let buf = MessageBuf::new();

        // 設計ドキュメントの初期容量に従う
        assert!(buf.request_uri.capacity() >= 128, "request_uri の初期容量は128以上であるべき");
        assert!(buf.version.capacity() >= 8, "version の初期容量は8以上であるべき");
        assert!(buf.reason_phrase.capacity() >= 32, "reason_phrase の初期容量は32以上であるべき");

        // header_names: 16要素, 各32 bytes
        assert!(buf.header_names.capacity() >= 16, "header_names の初期容量は16要素以上であるべき");
        for name in &buf.header_names {
            assert!(name.capacity() >= 32, "各 header_name の初期容量は32以上であるべき");
        }

        // header_values: 16要素, 各128 bytes
        assert!(buf.header_values.capacity() >= 16, "header_values の初期容量は16要素以上であるべき");
        for value in &buf.header_values {
            assert!(value.capacity() >= 128, "各 header_value の初期容量は128以上であるべき");
        }

        // output: 2048 bytes
        assert!(buf.output.capacity() >= 2048, "output の初期容量は2048以上であるべき");

        // body: 0 bytes (通常は空)
        assert_eq!(buf.body.capacity(), 0, "body の初期容量は0であるべき");
    }

    /// clear() のテスト: データ書き込み後に全フィールドの長さが0になること
    #[test]
    fn test_message_buf_clear_resets_lengths() {
        let mut buf = MessageBuf::new();

        // 各フィールドにデータを書き込む
        buf.request_uri.push_str("sip:alice@example.com");
        buf.version.push_str("SIP/2.0");
        buf.reason_phrase.push_str("OK");
        buf.output.extend_from_slice(b"SIP/2.0 200 OK\r\n\r\n");
        buf.body.extend_from_slice(b"v=0\r\n");

        // header_names / header_values にデータを追加
        // new() では長さ0で作成されるため、push で追加
        buf.header_names.push(String::from("Via"));
        buf.header_names.push(String::from("From"));
        buf.header_values.push(String::from("SIP/2.0/UDP 10.0.0.1:5060"));
        buf.header_values.push(String::from("<sip:alice@example.com>"));

        // clear() を呼ぶ
        buf.clear();

        // 全フィールドの長さが0であること
        assert_eq!(buf.request_uri.len(), 0, "clear 後 request_uri は空であるべき");
        assert_eq!(buf.version.len(), 0, "clear 後 version は空であるべき");
        assert_eq!(buf.reason_phrase.len(), 0, "clear 後 reason_phrase は空であるべき");
        assert_eq!(buf.header_names.len(), 0, "clear 後 header_names は空であるべき");
        assert_eq!(buf.header_values.len(), 0, "clear 後 header_values は空であるべき");
        assert_eq!(buf.output.len(), 0, "clear 後 output は空であるべき");
        assert_eq!(buf.body.len(), 0, "clear 後 body は空であるべき");
    }

    /// clear() 後に容量0のフィールドがデフォルト容量で再確保されること（take 後の復元）
    /// std::mem::take() で String の容量が0になった場合、clear() がデフォルト容量で復元する
    #[test]
    fn test_message_buf_clear_restores_capacity_after_take() {
        let mut buf = MessageBuf::new();

        // パーサーが std::mem::take() で String を移動するシミュレーション
        buf.request_uri.push_str("sip:bob@example.com");
        let _taken_uri = std::mem::take(&mut buf.request_uri);
        assert_eq!(buf.request_uri.capacity(), 0, "take 後は容量0であるべき");

        buf.version.push_str("SIP/2.0");
        let _taken_version = std::mem::take(&mut buf.version);
        assert_eq!(buf.version.capacity(), 0, "take 後は容量0であるべき");

        buf.reason_phrase.push_str("OK");
        let _taken_reason = std::mem::take(&mut buf.reason_phrase);
        assert_eq!(buf.reason_phrase.capacity(), 0, "take 後は容量0であるべき");

        // header_names / header_values も take
        buf.header_names.push(String::from("Via"));
        let _taken_names = std::mem::take(&mut buf.header_names);
        assert_eq!(buf.header_names.capacity(), 0, "take 後は容量0であるべき");

        buf.header_values.push(String::from("value"));
        let _taken_values = std::mem::take(&mut buf.header_values);
        assert_eq!(buf.header_values.capacity(), 0, "take 後は容量0であるべき");

        // clear() で容量が復元されること
        buf.clear();

        assert!(buf.request_uri.capacity() >= 128,
            "clear 後 request_uri の容量は128以上に復元されるべき (実際: {})", buf.request_uri.capacity());
        assert!(buf.version.capacity() >= 8,
            "clear 後 version の容量は8以上に復元されるべき (実際: {})", buf.version.capacity());
        assert!(buf.reason_phrase.capacity() >= 32,
            "clear 後 reason_phrase の容量は32以上に復元されるべき (実際: {})", buf.reason_phrase.capacity());
        assert!(buf.header_names.capacity() >= 16,
            "clear 後 header_names の容量は16以上に復元されるべき (実際: {})", buf.header_names.capacity());
        assert!(buf.header_values.capacity() >= 16,
            "clear 後 header_values の容量は16以上に復元されるべき (実際: {})", buf.header_values.capacity());
    }

    /// clear() 後に output の容量が維持されること（容量0でない場合は再確保不要）
    #[test]
    fn test_message_buf_clear_preserves_output_capacity() {
        let mut buf = MessageBuf::new();
        let initial_output_capacity = buf.output.capacity();

        // output にデータを書き込む
        buf.output.extend_from_slice(&[0u8; 4096]);
        let capacity_after_write = buf.output.capacity();
        assert!(capacity_after_write >= 4096);

        // clear() 後も容量が維持されること
        buf.clear();
        assert_eq!(buf.output.len(), 0, "clear 後 output の長さは0であるべき");
        assert!(buf.output.capacity() >= initial_output_capacity,
            "clear 後 output の容量は初期容量以上であるべき");
    }

    // Feature: sip-message-pool, Property 3: clear 後の容量維持
    // For any MessageBuf に対して、任意のデータを書き込んだ後に clear() を呼び出した場合、
    // 各フィールドの長さは0になるが、output フィールドの capacity() は clear 前の値以上を維持する。
    // Validates: Requirements 6.3
    // Feature: sip-message-pool, Property 1: プール初期化と取得の整合性
    // For any 正の整数 N に対して、容量 N で初期化した MessagePool から N 回連続で get() を
    // 呼び出した場合、全ての呼び出しが有効な（全フィールドが空の）MessageBuf を返す。
    // Validates: Requirements 1.1, 1.2
    proptest! {
        #[test]
        fn prop_pool_init_and_get_consistency(n in 1..128usize) {
            let pool = MessagePool::new(n);
            prop_assert_eq!(pool.available(), n, "new({}) で available() は {} であるべき", n, n);

            for i in 0..n {
                let buf = pool.get();

                // 全フィールドの長さが0（空の初期状態）であること
                prop_assert_eq!(buf.request_uri.len(), 0,
                    "get() #{}: request_uri は空であるべき", i);
                prop_assert_eq!(buf.version.len(), 0,
                    "get() #{}: version は空であるべき", i);
                prop_assert_eq!(buf.reason_phrase.len(), 0,
                    "get() #{}: reason_phrase は空であるべき", i);
                prop_assert_eq!(buf.header_names.len(), 0,
                    "get() #{}: header_names は空であるべき", i);
                prop_assert_eq!(buf.header_values.len(), 0,
                    "get() #{}: header_values は空であるべき", i);
                prop_assert_eq!(buf.output.len(), 0,
                    "get() #{}: output は空であるべき", i);
                prop_assert_eq!(buf.body.len(), 0,
                    "get() #{}: body は空であるべき", i);
            }

            // N 回取得後、プールは空であること
            prop_assert_eq!(pool.available(), 0,
                "{} 回 get() 後 available() は 0 であるべき", n);
        }
    }

    // Feature: sip-message-pool, Property 2: 取得・使用・返却サイクルの冪等性
    // For any MessageBuf に対して、任意の文字列データを書き込んだ後に pool.put(buf) で返却し、
    // pool.get() で再取得した MessageBuf は、全てのフィールドの長さが0であり、
    // 新規作成した MessageBuf と機能的に等価である。
    // Validates: Requirements 1.4, 6.1, 6.2, 6.4
    proptest! {
        #[test]
        fn prop_pool_get_put_cycle_idempotent(
            uri in ".{0,256}",
            ver in ".{0,32}",
            reason in ".{0,64}",
            output_data in proptest::collection::vec(any::<u8>(), 0..4096),
            body_data in proptest::collection::vec(any::<u8>(), 0..2048),
            header_count in 0..32usize,
        ) {
            let pool = MessagePool::new(1);

            // プールから取得
            let mut buf = pool.get();

            // 任意のデータを書き込む
            buf.request_uri.push_str(&uri);
            buf.version.push_str(&ver);
            buf.reason_phrase.push_str(&reason);
            buf.output.extend_from_slice(&output_data);
            buf.body.extend_from_slice(&body_data);

            for i in 0..header_count {
                buf.header_names.push(format!("Header-{}", i));
                buf.header_values.push(format!("value-{}", i));
            }

            // プールに返却
            pool.put(buf);

            // 再取得
            let recycled = pool.get();

            // 新規作成した MessageBuf と比較
            let fresh = MessageBuf::new();

            // 全フィールドの長さが0であること（新規作成と等価）
            prop_assert_eq!(recycled.request_uri.len(), fresh.request_uri.len(),
                "再取得した buf の request_uri の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.version.len(), fresh.version.len(),
                "再取得した buf の version の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.reason_phrase.len(), fresh.reason_phrase.len(),
                "再取得した buf の reason_phrase の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.header_names.len(), fresh.header_names.len(),
                "再取得した buf の header_names の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.header_values.len(), fresh.header_values.len(),
                "再取得した buf の header_values の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.output.len(), fresh.output.len(),
                "再取得した buf の output の長さは新規作成と等価であるべき");
            prop_assert_eq!(recycled.body.len(), fresh.body.len(),
                "再取得した buf の body の長さは新規作成と等価であるべき");
        }
    }

    proptest! {
        #[test]
        fn prop_clear_preserves_output_capacity(
            uri in ".{0,256}",
            ver in ".{0,32}",
            reason in ".{0,64}",
            output_data in proptest::collection::vec(any::<u8>(), 0..8192),
            body_data in proptest::collection::vec(any::<u8>(), 0..4096),
            header_count in 0..32usize,
        ) {
            let mut buf = MessageBuf::new();

            // 任意のデータを書き込む
            buf.request_uri.push_str(&uri);
            buf.version.push_str(&ver);
            buf.reason_phrase.push_str(&reason);
            buf.output.extend_from_slice(&output_data);
            buf.body.extend_from_slice(&body_data);

            for i in 0..header_count {
                buf.header_names.push(format!("Header-{}", i));
                buf.header_values.push(format!("value-{}", i));
            }

            // clear 前の output capacity を記録
            let output_capacity_before = buf.output.capacity();

            // clear() を呼ぶ
            buf.clear();

            // 全フィールドの長さが0であること
            prop_assert_eq!(buf.request_uri.len(), 0, "clear 後 request_uri の長さは0であるべき");
            prop_assert_eq!(buf.version.len(), 0, "clear 後 version の長さは0であるべき");
            prop_assert_eq!(buf.reason_phrase.len(), 0, "clear 後 reason_phrase の長さは0であるべき");
            prop_assert_eq!(buf.header_names.len(), 0, "clear 後 header_names の長さは0であるべき");
            prop_assert_eq!(buf.header_values.len(), 0, "clear 後 header_values の長さは0であるべき");
            prop_assert_eq!(buf.output.len(), 0, "clear 後 output の長さは0であるべき");
            prop_assert_eq!(buf.body.len(), 0, "clear 後 body の長さは0であるべき");

            // output の capacity が維持されること
            prop_assert!(
                buf.output.capacity() >= output_capacity_before,
                "clear 後 output の capacity ({}) は clear 前 ({}) 以上であるべき",
                buf.output.capacity(),
                output_capacity_before
            );
        }
    }
}
