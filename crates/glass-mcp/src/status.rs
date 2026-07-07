//! `glass-mcp status`: report whether a glass server is running at `addr` and
//! its endpoint, by reading the unauthenticated `/healthz` (loopback only).

/// Default `addr` when none is given — matches `serve`'s and `setup`'s default.
/// Not feature-gated (unlike `crate::serve::config::DEFAULT_ADDR`, which only exists
/// when the network-transport feature is compiled in — a doc link to it would be broken
/// without that feature): `status` always needs a default address regardless of which
/// optional features this build carries.
const DEFAULT_ADDR: &str = "127.0.0.1:7300";

pub(crate) fn run(addr: Option<&str>) -> anyhow::Result<()> {
    let addr = addr.unwrap_or(DEFAULT_ADDR);
    match crate::setup::fetch_health(addr) {
        Some(h) => {
            println!("glass: running");
            println!("endpoint: http://{addr}/");
            #[cfg(target_os = "macos")]
            println!(
                "grants: screen-recording {}, accessibility {}",
                yes_no(h.screen_recording),
                yes_no(h.accessibility)
            );
            let _ = &h;
            Ok(())
        }
        None => {
            println!("glass: not running (nothing answered /healthz at {addr})");
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "OK",
        Some(false) => "not granted",
        None => "unknown",
    }
}
