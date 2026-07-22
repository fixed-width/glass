//! End-to-end: drive glass-mcp over HTTP (the real MCP tool schema, not glass-core's library
//! API — see `common::mcp_ignore` for why) against a `--blink`ing glass-testapp under Xvfb, to
//! prove `ignore` regions work through the whole stack: parameter parsing, coordinate mapping,
//! and the settle/diff logic together. `#[ignore]`d; run via
//! `./scripts/test-x11.sh blink_region_settles_with_ignore_and_masks_diff`.

// This test needs one `unsafe { env::set_var }` for pre-spawn setup (see the `// SAFETY:` note
// below), so it opts out of the workspace `unsafe_code = "deny"` — same as network.rs.
#![allow(unsafe_code)]

mod common;

use common::mcp_ignore::assert_blink_region_e2e;
use common::Xvfb;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
async fn blink_region_settles_with_ignore_and_masks_diff() {
    let xvfb = Xvfb::start();
    // The x11 backend reads GLASS_DISPLAY (never ambient $DISPLAY).
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    assert_blink_region_e2e(TESTAPP, "x11", 5_000).await;
}
