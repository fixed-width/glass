//! End-to-end: the shipped `smoke` subcommand against a real app under the Wayland backend's
//! own headless sway. #[ignore]d (needs sway >=1.12 and a target app); run via:
//!   cargo test -p glass-mcp --test smoke_wayland -- --ignored

mod common;

use common::{REQUIRE_WAYLAND, assert_smoke_gate, sway_probe};

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
#[ignore = "requires sway >=1.12 and a target app; run via: cargo test -p glass-mcp --test smoke_wayland -- --ignored"]
fn smoke_wayland_passes_against_a_real_app() {
    // The backend spawns its own compositor, so there is nothing to start here — but a host
    // without sway must skip rather than report a failure it cannot act on.
    if !sway_probe(SERVER).can_run(REQUIRE_WAYLAND, "glass-discoverable sway >=1.12") {
        return;
    }
    assert_smoke_gate(SERVER, "wayland", &[]);
}
