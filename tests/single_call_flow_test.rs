use sip_load_test::dialog::DialogManager;
use sip_load_test::sip::message::SipMessage;
use sip_load_test::sip::parser::parse_sip_message;
use sip_load_test::stats::StatsCollector;
use sip_load_test::transport::UdpTransport;
use sip_load_test::uac::{Uac, UacConfig};
use sip_load_test::uas::{SipTransport, Uas, UasConfig};
use sip_load_test::user_pool::{UserEntry, UserPool, UsersFile};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

async fn wait_for_shutdown(flag: &AtomicBool) {
    loop {
        if flag.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn test_single_invite_call_completes() {
    // 1. Bind transports on ephemeral ports
    let addr: IpAddr = "127.0.0.1".parse().unwrap();

    let uac_transport = Arc::new(UdpTransport::bind(addr, 0, 1).await.unwrap());
    let uas_transport = Arc::new(UdpTransport::bind(addr, 0, 1).await.unwrap());

    let uac_local_addr = uac_transport.local_addr(0).expect("UAC local_addr");
    let uas_local_addr = uas_transport.local_addr(0).expect("UAS local_addr");

    eprintln!("UAC bound to: {}", uac_local_addr);
    eprintln!("UAS bound to: {}", uas_local_addr);

    // 2. Create user pool
    let users = UsersFile {
        users: vec![UserEntry {
            username: "testuser".to_string(),
            domain: "localhost".to_string(),
            password: "testpass".to_string(),
        }],
    };
    let user_pool = Arc::new(UserPool::from_users_file(users).unwrap());

    // 3. Create stats and dialog manager
    let stats = Arc::new(StatsCollector::new());
    let dialog_manager = Arc::new(DialogManager::new(100));

    // TODO: Complete integration test
    let _ = (stats, dialog_manager, user_pool, uac_transport, uas_transport, uac_local_addr, uas_local_addr);
}
