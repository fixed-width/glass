//! Smoke test for the Xvfb harness itself. #[ignore]d so the default
//! `cargo test` stays green without a display; run via scripts/test-x11.sh.

mod common;

use common::Xvfb;
use x11rb::connection::Connection;

#[test]
#[ignore = "requires Xvfb; run via scripts/test-x11.sh"]
fn xvfb_harness_starts_and_accepts_connections() {
    let xvfb = Xvfb::start();
    let (conn, screen_num) = x11rb::connect(Some(&xvfb.display)).unwrap();
    let root = conn.setup().roots[screen_num].root;
    assert_ne!(root, 0);
}
