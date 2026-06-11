//! `#[ignore]`d — needs `dbus-daemon` + `at-spi-bus-launcher`. Run via
//! `scripts/test-a11y-selfbus.sh` or directly with those tools installed.

use glass_dbus_linux::PrivateBus;

#[test]
#[ignore = "needs dbus-daemon + at-spi-bus-launcher"]
fn starts_yields_addresses_and_reaps() {
    let bus = PrivateBus::start().expect("start private bus");
    assert!(bus.session_bus_address().starts_with("unix:"), "session addr: {}", bus.session_bus_address());
    assert!(bus.a11y_bus_address().starts_with("unix:"), "a11y addr: {}", bus.a11y_bus_address());

    let addr = bus.a11y_bus_address().to_string();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = atspi::connection::AccessibilityConnection::from_address(
            addr.as_str().try_into().expect("valid address"),
        )
        .await
        .expect("connect to private a11y bus via from_address");
        let _root = conn.root_accessible_on_registry().await.expect("registry root");
    });
    drop(bus);
}

#[test]
#[ignore = "needs dbus-daemon + at-spi-bus-launcher"]
fn a11y_socket_is_private_not_host_runtime_dir() {
    let bus = glass_dbus_linux::PrivateBus::start().expect("start private bus");
    let addr = bus.a11y_bus_address();
    assert!(!addr.contains("/run/user/"), "a11y socket leaked into the host runtime dir: {addr}");
    assert!(addr.contains("/at-spi/"), "expected an at-spi socket path: {addr}");
}
