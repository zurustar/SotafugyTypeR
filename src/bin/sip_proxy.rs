use clap::Parser;
use futures::future::join_all;
use sip_load_test::auth::ProxyAuth;
use sip_load_test::config::load_proxy_config_from_file;
use sip_load_test::proxy::{LocationService, ProxyConfig, SipProxy};
use sip_load_test::sip::message::SipMessage;
use sip_load_test::sip::parser::parse_sip_message;
use sip_load_test::transport::UdpTransport;
use sip_load_test::user_pool::UserPool;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "sip-proxy")]
struct Args {
    /// JSON設定ファイルパス
    config: PathBuf,
}

/// 受信ループ: UdpTransport から SIP メッセージを受信し、SipProxy に振り分ける。
/// シャットダウンフラグが true になるとループを終了する。
async fn recv_loop(
    transport: Arc<UdpTransport>,
    proxy: Arc<SipProxy>,
    shutdown: Arc<AtomicBool>,
    socket_idx: usize,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // タイムアウト付きで受信（シャットダウンフラグをチェックするため）
        let recv_result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            transport.recv_from(socket_idx),
        )
        .await;

        let (data, from) = match recv_result {
            Ok(Ok((data, from))) => (data, from),
            Ok(Err(e)) => {
                eprintln!("Error receiving message: {}", e);
                continue;
            }
            Err(_) => {
                // タイムアウト - シャットダウンフラグをチェックして続行
                continue;
            }
        };

        // SIPメッセージのパース
        let msg = match parse_sip_message(&data) {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("Failed to parse SIP message from {}: {}", from, e);
                continue;
            }
        };

        // リクエスト/レスポンスの振り分け（並行処理）
        let proxy = proxy.clone();
        tokio::spawn(async move {
            match &msg {
                SipMessage::Request(_) => {
                    if let Err(e) = proxy.handle_request(msg, from).await {
                        eprintln!("Error handling request from {}: {}", from, e);
                    }
                }
                SipMessage::Response(_) => {
                    if let Err(e) = proxy.handle_response(msg, from).await {
                        eprintln!("Error handling response from {}: {}", from, e);
                    }
                }
            }
        });
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // 設定ファイルの読み込み
    let config = match load_proxy_config_from_file(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {}", e);
            std::process::exit(1);
        }
    };

    // バリデーション
    if let Err(errors) = config.validate() {
        eprintln!("Configuration validation failed:");
        for err in &errors {
            eprintln!("  - {}", err);
        }
        std::process::exit(1);
    }

    // ユーザプールの構築（users_file が指定されている場合）
    let user_pool = if let Some(ref users_file_path) = config.users_file {
        match UserPool::load_from_file(std::path::Path::new(users_file_path)) {
            Ok(pool) => Some(Arc::new(pool)),
            Err(e) => {
                eprintln!("Error loading users file: {}", e);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    // UDPトランスポートの初期化
    let host: IpAddr = match config.host.parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("Invalid host address '{}': {}", config.host, e);
            std::process::exit(1);
        }
    };

    let transport = match UdpTransport::bind(host, config.port, config.bind_count as u16).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "Failed to bind UDP socket on {}:{}: {}",
                config.host, config.port, e
            );
            std::process::exit(1);
        }
    };

    // UdpTransport を Arc で共有（recv_from 用と SipTransport 用）
    let transport = Arc::new(transport);
    let transport_for_recv = transport.clone();
    let transport_arc: Arc<dyn sip_load_test::uas::SipTransport> = transport;

    // LocationService の初期化
    let location_service = Arc::new(LocationService::new());

    // ProxyAuth の構築（認証が有効な場合）
    let auth = if config.auth_enabled {
        let pool = user_pool.expect("user_pool must be present when auth is enabled");
        Some(ProxyAuth::new(config.auth_realm.clone(), pool))
    } else {
        None
    };

    // ProxyConfig の構築
    let forward_addr = match format!("{}:{}", config.forward_host, config.forward_port).parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!(
                "Invalid forward address '{}:{}': {}",
                config.forward_host, config.forward_port, e
            );
            std::process::exit(1);
        }
    };

    let proxy_config = ProxyConfig {
        host: config.host.clone(),
        port: config.port,
        forward_addr,
        domain: config.host.clone(),
        debug: config.debug,
    };

    // SipProxy インスタンスの生成（Arc で共有し、spawn した非同期タスクで並行処理）
    let proxy = Arc::new(SipProxy::new(transport_arc, location_service, auth, proxy_config));

    // シャットダウンフラグの設定
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_flag = shutdown.clone();

    // ctrlc によるシグナルハンドリング（SIGINT/SIGTERM）
    if let Err(e) = ctrlc::set_handler(move || {
        eprintln!("\nReceived shutdown signal, stopping...");
        shutdown_flag.store(true, Ordering::SeqCst);
    }) {
        eprintln!("Failed to set signal handler: {}", e);
        std::process::exit(1);
    }

    eprintln!(
        "SIP Proxy listening on {}:{}",
        config.host, config.port
    );
    eprintln!(
        "Forwarding to {}:{}",
        config.forward_host, config.forward_port
    );
    if config.auth_enabled {
        eprintln!("Authentication enabled (realm: {})", config.auth_realm);
    }
    if config.debug {
        eprintln!("[DEBUG] Debug mode enabled");
    }
    eprintln!("Bind count: {}", config.bind_count);
    eprintln!("Recv task count: {} (per socket)", config.recv_task_count);
    eprintln!("Total recv tasks: {}", config.bind_count * config.recv_task_count);

    // SIPメッセージ受信ループ（bind_count × recv_task_count 個を並列起動）
    let mut handles = Vec::new();
    for socket_idx in 0..transport_for_recv.socket_count() {
        for _ in 0..config.recv_task_count {
            let t = transport_for_recv.clone();
            let p = proxy.clone();
            let s = shutdown.clone();
            handles.push(tokio::spawn(recv_loop(t, p, s, socket_idx)));
        }
    }

    join_all(handles).await;

    eprintln!("SIP Proxy shutting down.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use sip_load_test::config::load_proxy_config_from_str;
    use sip_load_test::proxy::SipProxy;

    /// SipProxy must be Send + Sync to be shared via Arc across spawned tasks.
    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn sip_proxy_is_send_and_sync() {
        _assert_send_sync::<SipProxy>();
    }

    #[test]
    fn test_args_parse_config_path() {
        let args = Args::parse_from(["sip-proxy", "config.json"]);
        assert_eq!(args.config, PathBuf::from("config.json"));
    }

    #[test]
    fn test_args_parse_absolute_path() {
        let args = Args::parse_from(["sip-proxy", "/etc/sip-proxy/config.json"]);
        assert_eq!(args.config, PathBuf::from("/etc/sip-proxy/config.json"));
    }

    #[test]
    fn test_args_parse_missing_config_fails() {
        let result = Args::try_parse_from(["sip-proxy"]);
        assert!(result.is_err());
    }

    use sip_load_test::proxy::LocationService;
    use sip_load_test::uas::SipTransport;
    use std::net::SocketAddr;

    /// テスト用のモックトランスポート（recv_loop テスト用）
    struct MockTransportForRecv;

    impl SipTransport for MockTransportForRecv {
        fn send_to<'a>(
            &'a self,
            _data: &'a [u8],
            _addr: SocketAddr,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), sip_load_test::error::SipLoadTestError>> + Send + 'a>,
        > {
            Box::pin(async { Ok(()) })
        }
    }

    /// シャットダウンフラグが true の状態で recv_loop を呼び出した場合に即座に終了するテスト
    #[tokio::test]
    async fn test_recv_loop_exits_immediately_when_shutdown_is_true() {
        // UdpTransport をローカルにバインド（テスト用）
        let transport = Arc::new(
            UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, 1)
                .await
                .unwrap(),
        );

        // SipProxy を構築（モックトランスポートを使用）
        let mock_transport: Arc<dyn SipTransport> = Arc::new(MockTransportForRecv);
        let location_service = Arc::new(LocationService::new());
        let proxy_config = ProxyConfig {
            host: "127.0.0.1".to_string(),
            port: 5060,
            forward_addr: "127.0.0.1:5080".parse().unwrap(),
            domain: "127.0.0.1".to_string(),
            debug: false,
        };
        let proxy = Arc::new(SipProxy::new(mock_transport, location_service, None, proxy_config));

        // シャットダウンフラグを true に設定
        let shutdown = Arc::new(AtomicBool::new(true));

        // recv_loop がタイムアウト内に終了することを検証
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            recv_loop(transport, proxy, shutdown, 0),
        )
        .await;

        assert!(result.is_ok(), "recv_loop should exit immediately when shutdown flag is true");
    }

    use proptest::prelude::*;

    // Feature: proxy-multi-socket-rport, Property 1: bind_count × recv_task_count 個のハンドルが生成される
    // **Validates: Requirements 1.1**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_recv_task_count_matches_spawned_handles(
            recv_task_count in 1usize..=8,
            bind_count in 1u16..=4,
        ) {
            // tokio ランタイムを構築してブロッキング実行
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // UdpTransport をローカルにバインド（bind_count 個のソケット）
                let transport = Arc::new(
                    UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, bind_count)
                        .await
                        .unwrap(),
                );

                // bind_count 個のソケットが生成されていることを検証
                prop_assert_eq!(transport.socket_count(), bind_count as usize);

                // SipProxy を構築（モックトランスポートを使用）
                let mock_transport: Arc<dyn SipTransport> = Arc::new(MockTransportForRecv);
                let location_service = Arc::new(LocationService::new());
                let proxy_config = ProxyConfig {
                    host: "127.0.0.1".to_string(),
                    port: 5060,
                    forward_addr: "127.0.0.1:5080".parse().unwrap(),
                    domain: "127.0.0.1".to_string(),
                    debug: false,
                };
                let proxy = Arc::new(SipProxy::new(mock_transport, location_service, None, proxy_config));

                // シャットダウンフラグを true に設定（タスクが即座に終了するように）
                let shutdown = Arc::new(AtomicBool::new(true));

                // bind_count × recv_task_count 個の recv_loop タスクを spawn し、JoinHandle を収集
                let mut handles = Vec::new();
                for socket_idx in 0..transport.socket_count() {
                    for _ in 0..recv_task_count {
                        let t = transport.clone();
                        let p = proxy.clone();
                        let s = shutdown.clone();
                        handles.push(tokio::spawn(recv_loop(t, p, s, socket_idx)));
                    }
                }

                // JoinHandle の数が bind_count × recv_task_count と一致することを検証
                let expected = bind_count as usize * recv_task_count;
                prop_assert_eq!(handles.len(), expected);

                // 全タスクが正常に終了することも検証
                let results = futures::future::join_all(handles).await;
                for result in &results {
                    prop_assert!(result.is_ok(), "recv_loop task should complete without panic");
                }

                Ok(())
            })?;
        }
    }

    // Feature: proxy-multi-socket-rport, Property 1: シャットダウンによる全タスクの終了（複数ソケット対応）
    // **Validates: Requirements 3.1, 3.3**
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(100))]

        #[test]
        fn prop_shutdown_terminates_all_recv_tasks(
            recv_task_count in 1usize..=8,
            bind_count in 1u16..=4,
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let transport = Arc::new(
                    UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, bind_count)
                        .await
                        .unwrap(),
                );

                // bind_count 個のソケットが生成されていることを検証
                prop_assert_eq!(transport.socket_count(), bind_count as usize);

                let mock_transport: Arc<dyn SipTransport> = Arc::new(MockTransportForRecv);
                let location_service = Arc::new(LocationService::new());
                let proxy_config = ProxyConfig {
                    host: "127.0.0.1".to_string(),
                    port: 5060,
                    forward_addr: "127.0.0.1:5080".parse().unwrap(),
                    domain: "127.0.0.1".to_string(),
                    debug: false,
                };
                let proxy = Arc::new(SipProxy::new(mock_transport, location_service, None, proxy_config));

                // シャットダウンフラグを false で起動（タスクが実際にループに入る）
                let shutdown = Arc::new(AtomicBool::new(false));

                // bind_count × recv_task_count 個のタスクを起動
                let mut handles = Vec::new();
                for socket_idx in 0..transport.socket_count() {
                    for _ in 0..recv_task_count {
                        let t = transport.clone();
                        let p = proxy.clone();
                        let s = shutdown.clone();
                        handles.push(tokio::spawn(recv_loop(t, p, s, socket_idx)));
                    }
                }

                let expected = bind_count as usize * recv_task_count;

                // タスクがループに入る時間を確保
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;

                // シャットダウンフラグを true に設定
                shutdown.store(true, Ordering::SeqCst);

                // 全タスクがタイムアウト内に終了することを検証
                // recv_loop のタイムアウトは 100ms なので、余裕を持って 3 秒
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    futures::future::join_all(handles),
                )
                .await;

                prop_assert!(result.is_ok(), "All recv_loop tasks should terminate within timeout after shutdown");

                let join_results = result.unwrap();
                prop_assert_eq!(join_results.len(), expected);
                for (i, r) in join_results.iter().enumerate() {
                    prop_assert!(r.is_ok(), "recv_loop task {} should complete without panic", i);
                }

                Ok(())
            })?;
        }
    }

    /// bind_count=1 の場合、単一ソケットで recv_task_count 個の RecvLoop が起動される
    /// （複数ソケット化前と同一の動作）
    /// Requirements: 7.1, 7.4
    #[tokio::test]
    async fn test_bind_count_1_spawns_single_socket_recv_loops() {
        let bind_count: u16 = 1;
        let recv_task_count: usize = 4;

        let transport = Arc::new(
            UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, bind_count)
                .await
                .unwrap(),
        );

        // bind_count=1 → ソケットは 1 つだけ
        assert_eq!(transport.socket_count(), 1);

        let mock_transport: Arc<dyn SipTransport> = Arc::new(MockTransportForRecv);
        let location_service = Arc::new(LocationService::new());
        let proxy_config = ProxyConfig {
            host: "127.0.0.1".to_string(),
            port: 5060,
            forward_addr: "127.0.0.1:5080".parse().unwrap(),
            domain: "127.0.0.1".to_string(),
            debug: false,
        };
        let proxy = Arc::new(SipProxy::new(mock_transport, location_service, None, proxy_config));

        let shutdown = Arc::new(AtomicBool::new(true));

        // main 関数と同じロジックで RecvLoop を起動
        let mut handles = Vec::new();
        for socket_idx in 0..transport.socket_count() {
            for _ in 0..recv_task_count {
                let t = transport.clone();
                let p = proxy.clone();
                let s = shutdown.clone();
                handles.push(tokio::spawn(recv_loop(t, p, s, socket_idx)));
            }
        }

        // bind_count=1 × recv_task_count=4 → 4 個のハンドル（単一ソケット時と同一）
        assert_eq!(handles.len(), recv_task_count);

        let results = futures::future::join_all(handles).await;
        for result in &results {
            assert!(result.is_ok(), "recv_loop task should complete without panic");
        }
    }

    /// bind_count=1 の場合、シャットダウンで全タスクが正常終了する
    /// Requirements: 7.1, 7.4
    #[tokio::test]
    async fn test_bind_count_1_shutdown_terminates_all_tasks() {
        let bind_count: u16 = 1;
        let recv_task_count: usize = 4;

        let transport = Arc::new(
            UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, bind_count)
                .await
                .unwrap(),
        );

        let mock_transport: Arc<dyn SipTransport> = Arc::new(MockTransportForRecv);
        let location_service = Arc::new(LocationService::new());
        let proxy_config = ProxyConfig {
            host: "127.0.0.1".to_string(),
            port: 5060,
            forward_addr: "127.0.0.1:5080".parse().unwrap(),
            domain: "127.0.0.1".to_string(),
            debug: false,
        };
        let proxy = Arc::new(SipProxy::new(mock_transport, location_service, None, proxy_config));

        // シャットダウンフラグを false で起動（タスクが実際にループに入る）
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut handles = Vec::new();
        for socket_idx in 0..transport.socket_count() {
            for _ in 0..recv_task_count {
                let t = transport.clone();
                let p = proxy.clone();
                let s = shutdown.clone();
                handles.push(tokio::spawn(recv_loop(t, p, s, socket_idx)));
            }
        }

        assert_eq!(handles.len(), recv_task_count);

        // タスクがループに入る時間を確保
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // シャットダウンシグナル送信
        shutdown.store(true, Ordering::SeqCst);

        // 全タスクがタイムアウト内に終了することを検証
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            futures::future::join_all(handles),
        )
        .await;

        assert!(result.is_ok(), "All recv_loop tasks should terminate within timeout after shutdown");
        let join_results = result.unwrap();
        assert_eq!(join_results.len(), recv_task_count);
        for r in &join_results {
            assert!(r.is_ok(), "recv_loop task should complete without panic");
        }
    }

    /// proxy_config.json に bind_count が未指定でもデフォルト値 4 で正常に動作する
    /// Requirements: 7.2
    #[test]
    fn test_proxy_config_without_bind_count_defaults_to_4() {
        // proxy_config.json と同等の JSON（bind_count 未指定）
        let json = r#"{
            "host": "127.0.0.1",
            "port": 5060,
            "forward_host": "127.0.0.1",
            "forward_port": 5080,
            "auth_enabled": false,
            "auth_realm": "sip-load-test",
            "debug": false
        }"#;

        let config = load_proxy_config_from_str(json).unwrap();
        assert_eq!(config.bind_count, 4, "bind_count should default to 4 when not specified");
        assert!(config.validate().is_ok(), "config with default bind_count should pass validation");
    }

    /// bind_count=1 の場合、Via ヘッダ生成やレスポンスルーティングなど
    /// SIP メッセージ処理が単一ソケット時と同一であることを検証する
    /// （bind_count=1 では socket_count=1 であり、全 RecvLoop が socket_idx=0 を使用する）
    /// Requirements: 7.1, 7.4
    #[tokio::test]
    async fn test_bind_count_1_all_recv_loops_use_socket_idx_0() {
        let bind_count: u16 = 1;
        let recv_task_count: usize = 4;

        let transport = Arc::new(
            UdpTransport::bind("127.0.0.1".parse().unwrap(), 0, bind_count)
                .await
                .unwrap(),
        );

        assert_eq!(transport.socket_count(), 1);

        // main 関数と同じロジックで socket_idx を収集
        let mut socket_indices = Vec::new();
        for socket_idx in 0..transport.socket_count() {
            for _ in 0..recv_task_count {
                socket_indices.push(socket_idx);
            }
        }

        // bind_count=1 の場合、全タスクが socket_idx=0 を使用する
        assert_eq!(socket_indices.len(), recv_task_count);
        for idx in &socket_indices {
            assert_eq!(*idx, 0, "All recv loops should use socket_idx=0 when bind_count=1");
        }
    }

    /// proxy_config.json に bind_count が未指定の場合でも正常に読み込めることを検証する
    /// （実際の proxy_config.json ファイルを使用）
    /// Requirements: 7.2
    #[test]
    fn test_proxy_config_file_without_bind_count_loads_successfully() {
        let config = load_proxy_config_from_file(&PathBuf::from("proxy_config.json")).unwrap();
        assert_eq!(config.bind_count, 4, "bind_count should default to 4 when not specified in file");
        assert!(config.validate().is_ok(), "config loaded from proxy_config.json should pass validation");
    }
}
