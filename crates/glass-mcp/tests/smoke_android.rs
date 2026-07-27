//! End-to-end: the shipped `smoke` subcommand against a stock app under the android backend, on a
//! live device. #[ignore]d (needs an adb-reachable device or an AVD to boot); run via:
//!   cargo test -p glass-mcp --test smoke_android -- --ignored

mod common;

use common::{REQUIRE_ANDROID, android_probe, assert_smoke_gate};

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
#[ignore = "requires an adb-reachable android device or an AVD to boot; run via: cargo test -p glass-mcp --test smoke_android -- --ignored"]
fn smoke_android_passes_against_a_stock_app() {
    // The backend attaches to an online device or boots the configured AVD, so there is nothing to
    // start here — but a host the probe found no way to get a device on must skip rather than
    // report a failure it cannot act on. A host with an AVD that glass refuses to use is reported.
    if !android_probe(SERVER).can_run(
        REQUIRE_ANDROID,
        "device glass will use, and no AVD it would boot instead",
    ) {
        return;
    }
    assert_smoke_gate(SERVER, "android", &[]);
}
