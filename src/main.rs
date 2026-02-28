use clap::Parser;
use sip_load_test::auth::DigestAuth;
use sip_load_test::cli::{run_compare, run_generate_users, Cli};
use sip_load_test::config::{self, calculate_max_dialogs_breakdown};
use sip_load_test::dialog::DialogManager;
use sip_load_test::orchestrator::Orchestrator;
use sip_load_test::reporter;
use sip_load_test::sip::parser::parse_sip_message;
use sip_load_test::transport::{SipTransport, UdpTransport};
use sip_load_test::uac::{Uac, UacConfig};
use sip_load_test::uas::Uas;
use sip_load_test::user_pool::UserPool;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Asynchronously waits until the given `AtomicBool` flag becomes `true`.
/// Uses a short polling interval to avoid busy-waiting while remaining responsive.
async fn wait_for_shutdown(flag: &AtomicBool) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli {
        Cli::GenerateUsers {
            prefix,
            start,
            count,
            domain,
            password_pattern,
            output,
            append,
        } => run_generate_users(&prefix, start, count, &domain, &password_pattern, &output, append),
        Cli::Run {
            config: config_path,
            mode,
            output,
        } => run_load_test(&config_path, mode, output.as_deref()).await,
        Cli::Compare { current, previous } => run_compare(&current, &previous),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

async fn run_load_test(
    config_path: &std::path::Path,
    mode: sip_load_test::config::RunMode,
    output: Option<&std::path::Path>,
) -> Result<(), sip_load_test::error::SipLoadTestError> {
    let mut cfg = config::load_from_file(config_path)?;
    cfg.mode = mode;

    let user_pool = if let Some(ref users_file_path) = cfg.users_file {
        Arc::new(UserPool::load_from_file(std::path::Path::new(users_file_path))?)
    } else {
        // Create a minimal default user pool when no users file is configured
        let default_users = sip_load_test::user_pool::UsersFile {
            users: vec![sip_load_test::user_pool::UserEntry {
                username: "default".to_string(),
                domain: "localhost".to_string(),
                password: "default".to_string(),
            }],
        };
        Arc::new(UserPool::from_users_file(default_users)?)
    };

    let mut orchestrator = Orchestrator::new(cfg.clone());
    orchestrator.setup_signal_handler()?;

    // --- Set up transport, UAC, UAS ---
    let uac_addr: IpAddr = cfg.uac_host.parse()
        .map_err(|e| sip_load_test::error::SipLoadTestError::ConfigError(
            format!("Invalid uac_host: {}", e)))?;
    let uas_addr: IpAddr = cfg.uas_host.parse()
        .map_err(|e| sip_load_test::error::SipLoadTestError::ConfigError(
            format!("Invalid uas_host: {}", e)))?;

    let uac_transport = UdpTransport::bind(uac_addr, cfg.uac_port, cfg.uac_port_count).await?;
    let uac_transport = Arc::new(uac_transport);

    let uas_transport = UdpTransport::bind(uas_addr, cfg.uas_port, cfg.uas_port_count).await?;
    let uas_transport = Arc::new(uas_transport);

    // Health check transport (reuse UAC transport)
    orchestrator.set_health_check_transport(uac_transport.clone() as Arc<dyn SipTransport>);

    let proxy_addr: std::net::SocketAddr = format!("{}:{}", cfg.proxy_host, cfg.proxy_port)
        .parse()
        .map_err(|e| sip_load_test::error::SipLoadTestError::ConfigError(
            format!("Invalid proxy address: {}", e)))?;

    let resolved_max_dialogs = match cfg.max_dialogs {
        Some(n) => {
            eprintln!("max_dialogs: {} (user-specified)", n);
            n
        }
        None => {
            let breakdown = calculate_max_dialogs_breakdown(&cfg);
            eprintln!(
                "max_dialogs: {} (auto-calculated: effective_cps={:.1}, call_duration={}, margin_factor={:.1})",
                breakdown.result,
                breakdown.effective_cps,
                breakdown.call_duration,
                breakdown.margin_factor,
            );
            breakdown.result
        }
    };
    let dialog_manager = Arc::new(DialogManager::new(resolved_max_dialogs));
    let stats = orchestrator.stats().clone();

    let uac_config = UacConfig {
        proxy_addr,
        local_addr: std::net::SocketAddr::new(uac_addr, cfg.uac_port),
        call_duration: Duration::from_secs(cfg.call_duration),
        dialog_timeout: Duration::from_secs(32),
        session_expires: Duration::from_secs(cfg.session_expires),
    };

    let mut uac = Uac::new(
        uac_transport.clone() as Arc<dyn SipTransport>,
        dialog_manager.clone(),
        stats.clone(),
        user_pool.clone(),
        uac_config,
    );

    if cfg.auth_enabled {
        uac.auth = Some(DigestAuth::new(user_pool.clone()));
    }

    let uac = Arc::new(uac);
    orchestrator.set_uac(uac.clone());

    let uas = Arc::new(Uas::new(
        uas_transport.clone() as Arc<dyn SipTransport>,
        stats.clone(),
    ));
    orchestrator.set_uas(uas.clone());

    // Spawn receive loops
    //
    // ## 受信メッセージ処理のバッチ化評価 (Task 14.3, Requirements 9.2)
    //
    // 評価結果: 現在の受信ループはソケットごとに1つの tokio::spawn タスクを生成し、
    // ループ内でメッセージをインライン処理している。メッセージごとの tokio::spawn は
    // 行っていないため、タスク生成オーバーヘッドは最小限である。
    //
    // プロキシ (src/proxy/mod.rs) も handle_request/handle_response 内で
    // インライン処理を行っており、個別タスク生成は行っていない。
    //
    // チャネルベースのバッチ処理ユーティリティは transport::batch モジュールに
    // 用意しており、将来的に極端な高負荷時のバックプレッシャー制御や
    // 優先度付きキューイングが必要になった場合に活用可能。
    let shutdown_flag = orchestrator.shutdown_flag().clone();

    // UAC receive loops: spawn one task per socket for parallel reception
    for idx in 0..uac_transport.socket_count() {
        let uac_recv = uac.clone();
        let uac_transport_recv = uac_transport.clone();
        let shutdown_uac = shutdown_flag.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = uac_transport_recv.recv_from(idx) => {
                        match result {
                            Ok((data, from)) => {
                                if let Ok(msg) = parse_sip_message(&data) {
                                    let _ = uac_recv.handle_response(msg, from).await;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = wait_for_shutdown(&shutdown_uac) => break,
                }
            }
        });
    }

    // UAS receive loops: spawn one task per socket for parallel reception
    for idx in 0..uas_transport.socket_count() {
        let uas_recv = uas.clone();
        let uas_transport_recv = uas_transport.clone();
        let shutdown_uas = shutdown_flag.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = uas_transport_recv.recv_from(idx) => {
                        match result {
                            Ok((data, from)) => {
                                if let Ok(msg) = parse_sip_message(&data) {
                                    let _ = uas_recv.handle_request(msg, from).await;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = wait_for_shutdown(&shutdown_uas) => break,
                }
            }
        });
    }

    // Spawn BYE batch loop: periodically sends BYE for expired Confirmed dialogs
    {
        let uac_bye = uac.clone();
        let call_duration = Duration::from_secs(cfg.call_duration);
        let shutdown_bye = shutdown_flag.clone();
        tokio::spawn(async move {
            uac_bye.start_bye_batch_loop(call_duration, shutdown_bye).await;
        });
    }

    // Run the experiment
    let experiment_result = orchestrator.run().await?;

    // Graceful shutdown
    orchestrator.graceful_shutdown().await?;

    if let Some(output_path) = output {
        reporter::write_json_result(&experiment_result, output_path)
            .map_err(|e| sip_load_test::error::SipLoadTestError::ConfigError(
                format!("Failed to write result file: {}", e),
            ))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wait_for_shutdown_returns_when_flag_is_already_true() {
        let flag = AtomicBool::new(true);
        // Should return immediately since flag is already true
        tokio::time::timeout(Duration::from_millis(100), wait_for_shutdown(&flag))
            .await
            .expect("wait_for_shutdown should return immediately when flag is true");
    }

    #[tokio::test]
    async fn test_wait_for_shutdown_returns_when_flag_becomes_true() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_setter = flag.clone();

        // Set the flag after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag_setter.store(true, Ordering::Relaxed);
        });

        // Should return once the flag is set
        tokio::time::timeout(Duration::from_secs(1), wait_for_shutdown(&flag))
            .await
            .expect("wait_for_shutdown should return when flag becomes true");
    }

    #[tokio::test]
    async fn test_wait_for_shutdown_does_not_return_while_flag_is_false() {
        let flag = AtomicBool::new(false);
        // Should NOT return within 50ms since flag stays false
        let result = tokio::time::timeout(Duration::from_millis(50), wait_for_shutdown(&flag)).await;
        assert!(result.is_err(), "wait_for_shutdown should not return while flag is false");
    }

    #[test]
    fn test_config_session_expires_converts_to_uac_config_duration() {
        // Requirement 2.2: Config.session_expires (u64) should be converted to UacConfig.session_expires (Duration)
        let cfg = sip_load_test::config::Config {
            session_expires: 600,
            ..Default::default()
        };
        let uac_config = UacConfig {
            proxy_addr: "127.0.0.1:5060".parse().unwrap(),
            local_addr: "127.0.0.1:5060".parse().unwrap(),
            call_duration: Duration::from_secs(cfg.call_duration),
            dialog_timeout: Duration::from_secs(32),
            session_expires: Duration::from_secs(cfg.session_expires),
        };
        assert_eq!(uac_config.session_expires, Duration::from_secs(600));
    }

    #[test]
    fn test_config_default_session_expires_converts_to_300_seconds() {
        // Requirement 2.2: Default Config.session_expires (300) converts to Duration::from_secs(300)
        let cfg = sip_load_test::config::Config::default();
        let uac_config = UacConfig {
            proxy_addr: "127.0.0.1:5060".parse().unwrap(),
            local_addr: "127.0.0.1:5060".parse().unwrap(),
            call_duration: Duration::from_secs(cfg.call_duration),
            dialog_timeout: Duration::from_secs(32),
            session_expires: Duration::from_secs(cfg.session_expires),
        };
        assert_eq!(uac_config.session_expires, Duration::from_secs(300));
    }
}
