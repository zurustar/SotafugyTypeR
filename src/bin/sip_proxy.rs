use clap::Parser;
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

    let transport = match UdpTransport::bind(host, config.port, 1).await {
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

    // SIPメッセージ受信ループ
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // タイムアウト付きで受信（シャットダウンフラグをチェックするため）
        let recv_result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            transport_for_recv.recv_from(0),
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

    eprintln!("SIP Proxy shutting down.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
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
}
