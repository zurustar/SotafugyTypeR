use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::error::SipLoadTestError;
use crate::sip::message::SipMessage;
use crate::sip::parser::parse_sip_message;
use crate::transport::SipTransport;

/// テスト用の共通モックトランスポート
/// - 送信メッセージの記録
/// - 送信カウント
/// - オプションの失敗注入
pub struct MockTransport {
    pub sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    pub send_count: AtomicUsize,
    pub should_fail: AtomicBool,
}

impl MockTransport {
    pub fn new() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            send_count: AtomicUsize::new(0),
            should_fail: AtomicBool::new(false),
        }
    }

    /// should_fail フラグを設定する
    pub fn set_should_fail(&self, fail: bool) {
        self.should_fail.store(fail, Ordering::Relaxed);
    }

    /// 送信成功したメッセージ数を返す（sent ベクタの長さ）
    pub fn sent_count(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    /// 送信されたメッセージをパースして返す
    pub fn sent_messages(&self) -> Vec<(SipMessage, SocketAddr)> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(data, addr)| {
                parse_sip_message(data).ok().map(|msg| (msg, *addr))
            })
            .collect()
    }
}

impl SipTransport for MockTransport {
    fn send_to<'a>(
        &'a self,
        data: &'a [u8],
        addr: SocketAddr,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SipLoadTestError>> + Send + 'a>>
    {
        Box::pin(async move {
            self.send_count.fetch_add(1, Ordering::Relaxed);
            if self.should_fail.load(Ordering::Relaxed) {
                return Err(SipLoadTestError::NetworkError(
                    std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "mock failure"),
                ));
            }
            self.sent.lock().unwrap().push((data.to_vec(), addr));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn addr() -> SocketAddr {
        "127.0.0.1:5060".parse().unwrap()
    }

    // --- メッセージ記録のテスト ---

    #[tokio::test]
    async fn mock_transport_records_sent_message() {
        let transport = MockTransport::new();
        let data = b"REGISTER sip:example.com SIP/2.0\r\n";
        let dest = addr();

        transport.send_to(data, dest).await.unwrap();

        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, data.to_vec());
        assert_eq!(sent[0].1, dest);
    }

    #[tokio::test]
    async fn mock_transport_records_multiple_messages() {
        let transport = MockTransport::new();
        let dest1: SocketAddr = "10.0.0.1:5060".parse().unwrap();
        let dest2: SocketAddr = "10.0.0.2:5061".parse().unwrap();

        transport.send_to(b"msg1", dest1).await.unwrap();
        transport.send_to(b"msg2", dest2).await.unwrap();

        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, b"msg1".to_vec());
        assert_eq!(sent[0].1, dest1);
        assert_eq!(sent[1].0, b"msg2".to_vec());
        assert_eq!(sent[1].1, dest2);
    }

    // --- 送信カウントのテスト ---

    #[tokio::test]
    async fn mock_transport_tracks_send_count() {
        let transport = MockTransport::new();
        let dest = addr();

        assert_eq!(transport.send_count.load(Ordering::Relaxed), 0);

        transport.send_to(b"a", dest).await.unwrap();
        assert_eq!(transport.send_count.load(Ordering::Relaxed), 1);

        transport.send_to(b"b", dest).await.unwrap();
        assert_eq!(transport.send_count.load(Ordering::Relaxed), 2);
    }

    // --- 失敗注入のテスト ---

    #[tokio::test]
    async fn mock_transport_returns_error_when_should_fail_is_true() {
        let transport = MockTransport::new();
        transport.should_fail.store(true, Ordering::Relaxed);

        let result = transport.send_to(b"data", addr()).await;
        assert!(result.is_err());

        // 失敗時はメッセージが記録されないこと
        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 0);
    }

    #[tokio::test]
    async fn mock_transport_succeeds_when_should_fail_is_false() {
        let transport = MockTransport::new();
        transport.should_fail.store(false, Ordering::Relaxed);

        let result = transport.send_to(b"data", addr()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn mock_transport_send_count_increments_even_on_failure() {
        let transport = MockTransport::new();
        transport.should_fail.store(true, Ordering::Relaxed);

        let _ = transport.send_to(b"data", addr()).await;
        assert_eq!(transport.send_count.load(Ordering::Relaxed), 1);
    }

    // --- new() のテスト ---

    #[test]
    fn mock_transport_new_initializes_empty() {
        let transport = MockTransport::new();
        assert_eq!(transport.sent.lock().unwrap().len(), 0);
        assert_eq!(transport.send_count.load(Ordering::Relaxed), 0);
        assert!(!transport.should_fail.load(Ordering::Relaxed));
    }

    // --- SipTransport トレイト互換性のテスト ---

    #[test]
    fn mock_transport_implements_sip_transport() {
        let transport = Arc::new(MockTransport::new());
        let _: Arc<dyn SipTransport> = transport;
    }

    // --- Property-Based Tests ---

    use proptest::prelude::*;
    use proptest::collection::vec as arb_vec;

    /// SocketAddr のジェネレータ
    fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
        (any::<[u8; 4]>(), 1u16..=65535u16).prop_map(|(ip, port)| {
            SocketAddr::from((ip, port))
        })
    }

    /// 送信操作（バイト列 + 宛先アドレス）のジェネレータ
    fn arb_send_op() -> impl Strategy<Value = (Vec<u8>, SocketAddr)> {
        (arb_vec(any::<u8>(), 0..256), arb_socket_addr())
    }

    // Feature: codebase-refactoring, Property 2: MockTransport message recording
    // **Validates: Requirements 6.3**
    proptest! {
        #[test]
        fn prop_mock_transport_records_all_messages(
            ops in arb_vec(arb_send_op(), 0..20)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = MockTransport::new();

                for (data, addr) in &ops {
                    transport.send_to(data, *addr).await.unwrap();
                }

                let sent = transport.sent.lock().unwrap();
                // 記録数が送信回数と一致する
                prop_assert_eq!(sent.len(), ops.len(),
                    "sent count mismatch: expected {}, got {}", ops.len(), sent.len());
                // 各メッセージの内容と宛先が送信時の値と一致する
                for (i, (data, addr)) in ops.iter().enumerate() {
                    prop_assert_eq!(&sent[i].0, data,
                        "data mismatch at index {}", i);
                    prop_assert_eq!(&sent[i].1, addr,
                        "addr mismatch at index {}", i);
                }
                // send_count もメッセージ数と一致する
                prop_assert_eq!(
                    transport.send_count.load(Ordering::Relaxed), ops.len(),
                    "send_count mismatch"
                );
                Ok(())
            })?;
        }

        #[test]
        fn prop_mock_transport_should_fail_returns_error(
            ops in arb_vec(arb_send_op(), 1..20)
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = MockTransport::new();
                transport.should_fail.store(true, Ordering::Relaxed);

                for (data, addr) in &ops {
                    let result = transport.send_to(data, *addr).await;
                    // should_fail が true の場合、全送信操作がエラーを返す
                    prop_assert!(result.is_err(),
                        "expected error but got Ok for data len={}", data.len());
                }

                // 失敗時はメッセージが記録されない
                let sent = transport.sent.lock().unwrap();
                prop_assert_eq!(sent.len(), 0,
                    "no messages should be recorded when should_fail is true");
                // send_count は失敗時もインクリメントされる
                prop_assert_eq!(
                    transport.send_count.load(Ordering::Relaxed), ops.len(),
                    "send_count should still increment on failure"
                );
                Ok(())
            })?;
        }

        #[test]
        fn prop_mock_transport_empty_and_large_data(
            data in prop_oneof![
                Just(vec![]),                          // 空のバイト列
                arb_vec(any::<u8>(), 1..10),           // 小さなバイト列
                arb_vec(any::<u8>(), 1000..2000),      // 大きなバイト列
            ],
            addr in arb_socket_addr()
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = MockTransport::new();

                transport.send_to(&data, addr).await.unwrap();

                let sent = transport.sent.lock().unwrap();
                prop_assert_eq!(sent.len(), 1);
                prop_assert_eq!(&sent[0].0, &data,
                    "data should be recorded exactly as sent (len={})", data.len());
                prop_assert_eq!(sent[0].1, addr);
                Ok(())
            })?;
        }
    }
}
