// Batch message processing module
//
// ## 受信メッセージ処理のバッチ化評価結果
//
// ### 評価対象
// - `src/main.rs` の受信ループ: ソケットごとに1つの `tokio::spawn` タスクを生成し、
//   ループ内でメッセージをインライン処理。メッセージごとの `tokio::spawn` は行っていない。
// - `src/proxy/mod.rs` の `handle_request`/`handle_response`: メッセージをインライン処理。
//   `tokio::spawn` による個別タスク生成は行っていない。
//
// ### 評価結論
// 現在のアーキテクチャは既にインライン処理を採用しており、メッセージごとの
// `tokio::spawn` オーバーヘッドは存在しない。そのため、チャネルベースのバッチ処理を
// 導入しても大きなスループット改善は見込めない。
//
// ただし、将来的に以下のケースでバッチ処理が有効になる可能性がある:
// - 極端な高負荷時（10万CPS以上）でのバックプレッシャー制御
// - メッセージの優先度付きキューイング
// - 複数ワーカーへの負荷分散
//
// 以下の `BatchMessageProcessor` は、将来の拡張に備えたチャネルベースの
// バッチ処理ユーティリティを提供する。

use std::net::SocketAddr;
use tokio::sync::mpsc;

/// 受信メッセージを表す構造体
#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    pub data: Vec<u8>,
    pub from: SocketAddr,
}

/// チャネルベースのバッチメッセージプロセッサ
///
/// 受信メッセージをチャネル経由で収集し、バッチ単位で処理する。
/// 現在のインライン処理アーキテクチャでは不要だが、将来の高負荷対応に備えて提供。
pub struct BatchMessageProcessor {
    batch_size: usize,
    rx: mpsc::Receiver<ReceivedMessage>,
}

/// バッチプロセッサの送信側ハンドル
#[derive(Clone)]
pub struct BatchSender {
    tx: mpsc::Sender<ReceivedMessage>,
}

impl BatchSender {
    /// メッセージを送信する。チャネルが満杯の場合はバックプレッシャーがかかる。
    pub async fn send(&self, msg: ReceivedMessage) -> Result<(), mpsc::error::SendError<ReceivedMessage>> {
        self.tx.send(msg).await
    }
}

/// バッチプロセッサとその送信側ハンドルを生成する。
///
/// - `batch_size`: 1回のバッチで処理するメッセージの最大数
/// - `channel_capacity`: チャネルのバッファサイズ
pub fn create_batch_processor(batch_size: usize, channel_capacity: usize) -> (BatchMessageProcessor, BatchSender) {
    let (tx, rx) = mpsc::channel(channel_capacity);
    let processor = BatchMessageProcessor { batch_size, rx };
    let sender = BatchSender { tx };
    (processor, sender)
}

impl BatchMessageProcessor {
    /// チャネルからメッセージをバッチ単位で取得する。
    ///
    /// 最低1件のメッセージが到着するまで待機し、その後 `batch_size` まで
    /// 非ブロッキングで追加メッセージを取得する。
    /// チャネルがクローズされた場合は `None` を返す。
    pub async fn recv_batch(&mut self) -> Option<Vec<ReceivedMessage>> {
        // 最初の1件は待機
        let first = self.rx.recv().await?;
        let mut batch = Vec::with_capacity(self.batch_size);
        batch.push(first);

        // 残りは非ブロッキングで取得
        while batch.len() < self.batch_size {
            match self.rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(_) => break,
            }
        }

        Some(batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 5060)
    }

    fn make_msg(id: u8) -> ReceivedMessage {
        ReceivedMessage {
            data: vec![id],
            from: test_addr(),
        }
    }

    #[tokio::test]
    async fn test_recv_batch_returns_single_message() {
        let (mut processor, sender) = create_batch_processor(10, 100);

        sender.send(make_msg(1)).await.unwrap();
        drop(sender);

        let batch = processor.recv_batch().await.unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].data, vec![1]);
    }

    #[tokio::test]
    async fn test_recv_batch_collects_up_to_batch_size() {
        let (mut processor, sender) = create_batch_processor(3, 100);

        // Send 5 messages, batch_size is 3
        for i in 0..5u8 {
            sender.send(make_msg(i)).await.unwrap();
        }

        // Allow messages to be buffered
        tokio::task::yield_now().await;

        let batch = processor.recv_batch().await.unwrap();
        assert!(batch.len() <= 3, "batch should not exceed batch_size");
        assert!(!batch.is_empty(), "batch should have at least 1 message");
    }

    #[tokio::test]
    async fn test_recv_batch_returns_none_when_channel_closed() {
        let (mut processor, sender) = create_batch_processor(10, 100);
        drop(sender);

        let result = processor.recv_batch().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recv_batch_processes_all_messages_across_batches() {
        let (mut processor, sender) = create_batch_processor(2, 100);
        let total = 5u8;

        for i in 0..total {
            sender.send(make_msg(i)).await.unwrap();
        }
        drop(sender);

        // Allow messages to be buffered
        tokio::task::yield_now().await;

        let mut all_data: Vec<u8> = Vec::new();
        while let Some(batch) = processor.recv_batch().await {
            assert!(batch.len() <= 2, "each batch should respect batch_size");
            for msg in batch {
                all_data.extend_from_slice(&msg.data);
            }
        }

        // All messages should be processed
        assert_eq!(all_data.len(), total as usize);
        for i in 0..total {
            assert!(all_data.contains(&i), "message {} should be processed", i);
        }
    }

    #[tokio::test]
    async fn test_batch_sender_clone_works() {
        let (mut processor, sender) = create_batch_processor(10, 100);
        let sender2 = sender.clone();

        sender.send(make_msg(1)).await.unwrap();
        sender2.send(make_msg(2)).await.unwrap();
        drop(sender);
        drop(sender2);

        tokio::task::yield_now().await;

        let batch = processor.recv_batch().await.unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[tokio::test]
    async fn test_batch_sender_backpressure_when_channel_full() {
        // Channel capacity of 1
        let (_processor, sender) = create_batch_processor(10, 1);

        // First send should succeed
        sender.send(make_msg(1)).await.unwrap();

        // Second send should block (channel full), so use try_send to verify
        let result = sender.tx.try_send(make_msg(2));
        assert!(result.is_err(), "channel should be full");
    }

    #[tokio::test]
    async fn test_received_message_preserves_source_address() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)), 9999);
        let msg = ReceivedMessage {
            data: vec![0xAB, 0xCD],
            from: addr,
        };

        let (mut processor, sender) = create_batch_processor(10, 100);
        sender.send(msg).await.unwrap();
        drop(sender);

        let batch = processor.recv_batch().await.unwrap();
        assert_eq!(batch[0].from, addr);
        assert_eq!(batch[0].data, vec![0xAB, 0xCD]);
    }
}
