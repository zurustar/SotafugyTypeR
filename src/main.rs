use clap::Parser;
use sip_load_test::auth::DigestAuth;
use sip_load_test::cli::{run_compare, run_generate_users, Cli};
use sip_load_test::config;
use sip_load_test::dialog::DialogManager;
use sip_load_test::orchestrator::Orchestrator;
use sip_load_test::reporter;
use sip_load_test::sip::parser::parse_sip_message;
use sip_load_test::transport::UdpTransport;
use sip_load_test::uac::{Uac, UacConfig};
use sip_load_test::uas::{Uas, SipTransport, UasConfig};
use sip_load_test::user_pool::UserPool;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

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

    let mut orchestrator = Orchestrator::new(cfg.clone(), user_pool.clone());
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

    let dialog_manager = Arc::new(DialogManager::new(cfg.max_dialogs));
    let stats = orchestrator.stats().clone();

    let uac_config = UacConfig {
        proxy_addr,
        call_duration: Duration::from_secs(cfg.call_duration),
        dialog_timeout: Duration::from_secs(32),
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
        UasConfig::default(),
    ));
    orchestrator.set_uas(uas.clone());

    // Spawn receive loops
    let shutdown_flag = orchestrator.shutdown_flag().clone();

    // UAC receive loop: dispatch responses to UAC
    let uac_recv = uac.clone();
    let uac_transport_recv = uac_transport.clone();
    let shutdown_uac = shutdown_flag.clone();
    tokio::spawn(async move {
        loop {
            if shutdown_uac.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match tokio::time::timeout(
                Duration::from_millis(100),
                uac_transport_recv.recv_from(0),
            ).await {
                Ok(Ok((data, from))) => {
                    if let Ok(msg) = parse_sip_message(&data) {
                        let _ = uac_recv.handle_response(msg, from).await;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => continue, // timeout, check shutdown flag
            }
        }
    });

    // UAS receive loop: dispatch requests to UAS
    let uas_recv = uas.clone();
    let uas_transport_recv = uas_transport.clone();
    let shutdown_uas = shutdown_flag.clone();
    tokio::spawn(async move {
        loop {
            if shutdown_uas.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            match tokio::time::timeout(
                Duration::from_millis(100),
                uas_transport_recv.recv_from(0),
            ).await {
                Ok(Ok((data, from))) => {
                    if let Ok(msg) = parse_sip_message(&data) {
                        let _ = uas_recv.handle_request(msg, from).await;
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => continue,
            }
        }
    });

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
