//! macOS menu-bar mode: GlassMcp.app as a visible NSStatusItem that serves MCP on a
//! background task. The daemon is never hidden — the thing serving is the thing showing
//! `glass ●` in the menu bar, with the endpoint, "Copy endpoint", "Restart", and "Quit glass".
//!
//! `#[cfg(feature = "network")]` at the module boundary (see `lib.rs`): `run` takes an
//! already-resolved [`ServeConfig`], which only exists when the network transport is
//! compiled in — matches `crate::serve`, which is gated the same way.
//!
//! ## Threading model
//!
//! `main.rs` is `#[tokio::main]` over the multi-thread runtime: `Runtime::block_on` polls the
//! async body on the process's real main thread ("thread 0"), and `tokio::spawn`ed tasks run
//! on worker threads. The `Serve` arm calls [`run`] synchronously (no `.await`), so it too
//! executes on thread 0 — the thread `glass_macos::init_main_thread()` (the first statement of
//! `main`) already used to establish the `NSApplication.sharedApplication` WindowServer
//! connection (see `glass-macos/src/ffi.rs`'s `thread0` notes for why one main-thread init is
//! sufficient and sound). Given that, menu-bar mode:
//!
//! 1. Runs the fail-closed exposure check and binds the listener (synchronously, with
//!    `std::net`), then registers it with the current tokio reactor via `TcpListener::from_std`
//!    — both valid here because thread 0 is inside the runtime context while polling the body.
//! 2. `tokio::spawn`s `serve::run_on(listener, …)` so the server runs on worker threads.
//! 3. Hands control to `glass_macos::menubar::run`, which builds the `NSStatusItem`/`NSMenu`
//!    via `MainThreadMarker` (requires thread 0 — it is) and blocks thread 0 on
//!    `NSApplication::run`. The server keeps serving on the workers while thread 0 pumps the
//!    AppKit event loop.
//! 4. "Quit glass" sends `terminate:` to `NSApp`, which exits the process (dropping the server
//!    task with it).

use crate::serve::config::ServeConfig;

#[cfg(target_os = "macos")]
pub fn run(cfg: ServeConfig) -> anyhow::Result<()> {
    macos::run(cfg)
}

#[cfg(not(target_os = "macos"))]
pub fn run(_cfg: ServeConfig) -> anyhow::Result<()> {
    anyhow::bail!("the menu-bar app is macOS-only")
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::ErrorKind;

    use super::ServeConfig;
    use crate::serve;

    pub fn run(cfg: ServeConfig) -> anyhow::Result<()> {
        // Fail-closed exposure rule (spec D4), the *same* check `serve::run` applies before
        // binding: a menu-bar serve on a network-exposed address must honor it too. Runs
        // before we touch a socket.
        serve::check_exposure(&cfg)?;

        let endpoint = format!("http://{}/", cfg.addr);

        // Bind synchronously with `std::net` (no async context needed for the bind itself),
        // then hand the socket to tokio. `AddrInUse` is surfaced *in the menu* rather than
        // killing the app — a menu bar that silently never appears would be the worst outcome.
        let (status_line, listener) = match std::net::TcpListener::bind(cfg.addr) {
            Ok(l) => (endpoint.clone(), Some(l)),
            Err(e) if e.kind() == ErrorKind::AddrInUse => (
                format!("another glass is already serving on {}", cfg.addr),
                None,
            ),
            Err(e) => return Err(anyhow::anyhow!("binding {}: {e}", cfg.addr)),
        };

        if let Some(std_listener) = listener {
            // We're on the `#[tokio::main]` block_on thread, so the runtime context is entered:
            // `TcpListener::from_std` (reactor registration) and `tokio::spawn` both work here.
            // VERIFY on-box: with the main thread blocked in `NSApplication::run`, the
            // multi-thread runtime's worker threads must keep driving the I/O reactor + this
            // spawned task — confirm the server actually accepts MCP connections while the menu
            // bar is up (the whole premise of the visible daemon).
            std_listener
                .set_nonblocking(true)
                .map_err(|e| anyhow::anyhow!("making the listener non-blocking: {e}"))?;
            let listener = tokio::net::TcpListener::from_std(std_listener).map_err(|e| {
                anyhow::anyhow!("registering the listener with the tokio reactor: {e}")
            })?;

            // Menu-bar mode resolves its audit sink from the environment (`GLASS_AUDIT_LOG` et
            // al.) — the LaunchAgent plist launches `--menubar --http` with no `--audit-log`
            // CLI flag to thread through. Fail-closed: an unopenable audit log aborts here.
            let (sink, report) = crate::audit::resolve(None, |k| std::env::var(k).ok())?;
            let glass = crate::boot(sink);

            tokio::spawn(async move {
                // Never swallow the server task's failure silently — surface it to the
                // LaunchAgent's stderr log. (The menu bar stays up so the operator still has a
                // Quit; wiring the failure into the status line is a future refinement.)
                if let Err(e) = serve::run_on(listener, cfg, glass, report).await {
                    eprintln!("glass: menu-bar server task exited with error: {e:#}");
                }
            });
        }

        // Build the menu bar and block the main thread on the AppKit run loop. The two
        // actionable items reuse glass-mcp's existing, validated helpers (`pbcopy` and the
        // LaunchAgent `bootout`+`bootstrap`); their errors go loudly to stderr (the menu action
        // has no dialog surface), matching onboarding's best-effort osascript fallback.
        let endpoint_for_copy = endpoint;
        glass_macos::menubar::run(glass_macos::menubar::MenuBarActions {
            title: "glass \u{25CF}".to_string(),
            status_line,
            on_copy: Box::new(move || {
                if let Err(e) = crate::onboarding::copy_to_clipboard(&endpoint_for_copy) {
                    eprintln!("glass: menu 'Copy endpoint' failed: {e}");
                }
            }),
            on_restart: Box::new(|| {
                // VERIFY on-box: this process *is* the LaunchAgent job, so
                // `restart_launch_agent`'s `launchctl bootout gui/<uid>/tech.fixedwidth.glass`
                // asks launchd to tear *us* down — there's a race where SIGTERM lands before the
                // following `bootstrap` runs, leaving the agent down (KeepAlive=false). Confirm a
                // menu "Restart" actually comes back up; if not, switch to a detached re-bootstrap
                // helper that runs after this process exits.
                if let Err(e) = crate::setup::restart_launch_agent() {
                    eprintln!("glass: menu 'Restart' failed: {e}");
                }
            }),
        })?;
        Ok(())
    }
}
