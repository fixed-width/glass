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
//! 0. Self-onboards first if a needed TCC grant is missing at startup (revoked in System
//!    Settings, or a periodic re-consent prompt not yet answered): shows the permission
//!    checklist and returns *without* binding, so it never serves blind. Its "Re-check"
//!    restarts this job in place (`launchctl kickstart -k`) so a fresh process re-reads the
//!    grants. The steps below run only once both grants are held.
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

    use glass_macos::onboarding_window::{run_checklist, ChecklistActions};

    use super::ServeConfig;
    use crate::serve;

    pub fn run(cfg: ServeConfig) -> anyhow::Result<()> {
        // Self-onboard fallback: the LaunchAgent starts unconditionally at login, but a grant it
        // needs can be missing right now — revoked in System Settings, or macOS 26's periodic
        // re-consent prompt not yet answered. Serving in that state would silently fail every
        // capture/input, so before touching a socket, show the *same* permission checklist the
        // first-run onboarder does (reusing its exact row widgets) instead of serving blind.
        //
        // TCC grants are cached per-process at launch, so a newly-granted permission only
        // becomes visible in a fresh process. "Re-check" therefore restarts THIS LaunchAgent job
        // in place — `launchctl kickstart -k` via `restart_launch_agent` — NOT the onboarder's
        // `open -n` relaunch (`relaunch` `exit(0)`s the caller, which here would kill the running
        // server). Once the restarted job reads both grants, it skips this branch and serves.
        if !(glass_macos::accessibility_granted() && glass_macos::screen_recording_granted()) {
            let actions = ChecklistActions {
                rows: crate::onboarding::grant_row_widgets(),
                on_recheck: Box::new(|| {
                    // Restart in place so a fresh process re-reads TCC. On failure the job stays
                    // up with the checklist open, so surface why "Re-check" did nothing rather
                    // than swallow it (the menu/checklist has no error surface of its own).
                    if let Err(e) = crate::setup::restart_launch_agent() {
                        eprintln!("glass: self-onboard 'Re-check' (restart) failed: {e}");
                    }
                }),
            };
            return run_checklist(actions).map_err(|e| anyhow::anyhow!(e));
        }

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
        // LaunchAgent `kickstart -k`); their errors go loudly to stderr (the menu action
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
                // This process *is* the LaunchAgent job, so `restart_launch_agent` uses
                // `launchctl kickstart -k gui/<uid>/tech.fixedwidth.glass` rather than
                // `bootout`+`bootstrap`: `kickstart -k` is a single request to launchd (a
                // separate supervisor process) to kill-and-restart the job in place, so it's
                // safe to call from inside the job being restarted and doesn't depend on
                // `KeepAlive`. `bootout` would instead SIGTERM this very process, and with
                // `KeepAlive=false` launchd would never bring it back.
                if let Err(e) = crate::setup::restart_launch_agent() {
                    eprintln!("glass: menu 'Restart' failed: {e}");
                }
            }),
            on_uninstall: Box::new(|| {
                // Confirm first, defaulting to Cancel so a stray click/Return never uninstalls.
                // `uninstall_launch_agent` boots out (SIGTERM) this very process, so a dialog
                // shown *after* it may not survive to be read — put the "drag to Trash" step in
                // the confirmation itself. Only the exact "Uninstall" button proceeds.
                let clicked = confirm_dialog(
                    "Remove glass?\n\nThis stops glass from launching at login and shuts it \
                     down now. To finish removing it, drag GlassMcp.app to the Trash.",
                    "Uninstall",
                );
                if clicked == "Uninstall" {
                    // Best-effort: a menu action has no further error surface, so log loudly to
                    // stderr rather than crash the action if bootout/plist-removal fails. On
                    // success this process is terminated as part of the uninstall.
                    if let Err(e) = crate::setup::uninstall_launch_agent() {
                        eprintln!("glass: menu 'Uninstall glass' failed: {e}");
                    }
                }
            }),
        })?;
        Ok(())
    }

    /// Show a modal two-button confirm dialog via `osascript` and return the title of the
    /// button the user clicked — or an empty string if they cancelled, dismissed it, or the
    /// shell-out failed. The buttons are **Cancel** and `confirm_button`, with **Cancel** the
    /// default so a stray Return/Enter never confirms.
    ///
    /// `message` and `confirm_button` are passed to the AppleScript as `argv` arguments (via
    /// `on run argv` — everything after the `-e` script becomes the run handler's `argv`),
    /// never interpolated into the script source, so untrusted text can't break the string
    /// quoting or inject AppleScript. Clicking the literal "Cancel" button raises AppleScript's
    /// `-128` (user canceled), which exits `osascript` non-zero with empty stdout — reported
    /// here as "not confirmed" (empty string), never a silent confirm.
    ///
    /// Both current arguments are controlled literals that never begin with `-`, so no `--`
    /// option terminator is passed (osascript's handling of a bare `--` in `argv` is version-
    /// dependent; omitting it keeps the `argv` indices exact).
    fn confirm_dialog(message: &str, confirm_button: &str) -> String {
        use std::process::Command;
        let script = r#"on run argv
    display dialog (item 1 of argv) buttons {"Cancel", (item 2 of argv)} default button "Cancel" with icon caution
    return button returned of result
end run"#;
        match Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .arg(message)
            .arg(confirm_button)
            .output()
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim().to_string(),
            Err(e) => {
                eprintln!("glass: confirm dialog (osascript) failed to run: {e}");
                String::new()
            }
        }
    }
}
