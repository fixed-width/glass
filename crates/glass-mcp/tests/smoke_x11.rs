//! End-to-end: the shipped `smoke` subcommand against a real app under Xvfb.
//! #[ignore]d (needs Xvfb + a target app); run via:
//!   cargo test -p glass-mcp --test smoke_x11 -- --ignored

mod common;

use common::{Xvfb, assert_smoke_gate};

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
#[ignore = "requires Xvfb and a target app; run via: cargo test -p glass-mcp --test smoke_x11 -- --ignored"]
fn smoke_x11_passes_against_a_real_app() {
    let xvfb = Xvfb::start();
    assert_smoke_gate(SERVER, "x11", &[("DISPLAY", xvfb.display.as_str())]);
}
