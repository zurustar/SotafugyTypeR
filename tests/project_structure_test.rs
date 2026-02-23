/// Verify that all modules are accessible from the crate root.
/// This test ensures the project structure is correctly set up.
/// Each `use` statement will cause a compile error if the module is missing.

#[allow(unused_imports)]
use sip_load_test::sip;
#[allow(unused_imports)]
use sip_load_test::transport;
#[allow(unused_imports)]
use sip_load_test::dialog;
#[allow(unused_imports)]
use sip_load_test::uac;
#[allow(unused_imports)]
use sip_load_test::uas;
#[allow(unused_imports)]
use sip_load_test::proxy;
#[allow(unused_imports)]
use sip_load_test::auth;
#[allow(unused_imports)]
use sip_load_test::stats;
#[allow(unused_imports)]
use sip_load_test::orchestrator;
#[allow(unused_imports)]
use sip_load_test::config;
#[allow(unused_imports)]
use sip_load_test::reporter;
#[allow(unused_imports)]
use sip_load_test::user_pool;

#[test]
fn all_modules_are_accessible() {
    // If this test compiles, all 12 modules are correctly declared.
    // sip module should also expose parser, formatter, message submodules.
    assert!(true);
}

#[test]
fn cargo_toml_defines_sip_proxy_binary() {
    let cargo_toml = std::fs::read_to_string("Cargo.toml").expect("Failed to read Cargo.toml");
    assert!(
        cargo_toml.contains("name = \"sip-proxy\""),
        "Cargo.toml should define sip-proxy binary"
    );
    assert!(
        cargo_toml.contains("path = \"src/bin/sip_proxy.rs\""),
        "Cargo.toml should specify path for sip-proxy binary"
    );
}
