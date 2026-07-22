//! End-to-end: the Wayland-backend twin of `ignore_regions_e2e.rs` — same assertion, same real
//! glass-mcp server over HTTP, against sway/Xwayland instead of Xvfb (no private display setup
//! needed here; `WaylandPlatform` manages its own compositor). The diff/settle path is shared
//! with X11, so a failure here that X11 doesn't reproduce points at the fixture or the backend,
//! not the mask. `#[ignore]`d; run via
//! `./scripts/test-wayland.sh blink_region_settles_with_ignore_and_masks_diff`.

mod common;

use common::mcp_ignore::assert_blink_region_e2e;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires sway; run via scripts/test-wayland.sh"]
async fn blink_region_settles_with_ignore_and_masks_diff() {
    // A freshly-spawned sway + Xwayland needs longer to come up than Xvfb; mirrors
    // wayland.rs's APP_TIMEOUT_MS.
    assert_blink_region_e2e(TESTAPP, "wayland", 15_000).await;
}
