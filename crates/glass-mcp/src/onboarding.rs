//! Double-click first-run for GlassMcp.app. Launched by LaunchServices with no subcommand
//! (see `main.rs`'s launch routing), the app is its own TCC responsible process, so the grant
//! requests it raises attribute to GlassMcp.app itself (a terminal spawning `setup` would key
//! the grant to the terminal instead).
//!
//! The flow ([`run`]) has three outcomes, cheapest first:
//!
//! - **A. Already running.** A healthy LaunchAgent is already serving with both grants held —
//!   a returning user. Nothing to onboard: don't re-request grants or reinstall over a working
//!   agent, just exit.
//! - **B. Ready to install.** Both grants are held in *this launch's* TCC snapshot, but no agent
//!   is serving yet. Install + start the menu-bar LaunchAgent (the fresh process that will read
//!   the grants and serve), then exit — the LaunchAgent, not this onboarder, becomes the running
//!   server.
//! - **C. Show the checklist.** At least one grant is missing. Show the permission-checklist
//!   window ([`glass_macos::run_checklist`]): one row per permission with its live snapshot, an
//!   "Open Settings" button that requests that permission (adding GlassMcp.app to its Privacy
//!   pane + prompting) and opens its pane, and a "Re-check" button that relaunches a *fresh*
//!   process. TCC grants are cached per-process at launch, so re-reading them requires a new
//!   process — the relaunched instance re-enters this flow and, once both grants read granted,
//!   lands in outcome B.
//!
//! Client-agnostic: the checklist only guides the grants; per-client MCP wiring is documented,
//! not assumed. macOS-only — there is no LaunchServices double-click hand-off to onboard
//! anywhere else.

/// The address [`run`] binds the onboarded LaunchAgent to when the caller has no more specific
/// preference — a LaunchServices double-click never passes one. Not feature-gated (unlike
/// `crate::serve::config::DEFAULT_ADDR`, which only exists when the network-transport feature is
/// compiled in — a doc link to it would be broken without that feature, so this is a plain code
/// reference): onboarding always needs an address to hand to [`crate::setup`] regardless of which
/// optional features this build carries. Matches the shipped LaunchAgent plist template and the
/// other `127.0.0.1:7300` defaults in `setup.rs`/`serve/config.rs`.
pub const DEFAULT_ADDR: &str = "127.0.0.1:7300";

// The checklist-window types live in glass-macos's `onboarding_window` module (not re-exported
// at the crate root, unlike the `request_*`/`*_granted`/`open_pane` predicates); import them here
// so `run`/`grant_row_widgets` name them unqualified. macOS-gated: the module is macOS-only.
#[cfg(target_os = "macos")]
use glass_macos::onboarding_window::{run_checklist, ChecklistActions, GrantRow};

/// Onboarding entry — see the module doc for the full A/B/C flow. Short-circuit if a healthy
/// agent is already serving with both grants (A); otherwise, if both grants are held in this
/// launch's snapshot, install + start the LaunchAgent and exit (B); otherwise show the
/// permission checklist (C). macOS-only — an error off macOS, since the double-click hand-off
/// only exists there.
pub fn run(addr: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // A. A healthy agent is already serving with both grants held — nothing to onboard.
        //    Don't re-request grants (would pop dialogs for nothing) or reinstall over a working
        //    agent.
        if crate::setup::fetch_health(addr).is_some_and(|h| h.grants_ready()) {
            return Ok(());
        }

        // B. Both grants held in THIS launch's snapshot, but nothing serving yet — install +
        //    start the menu-bar LaunchAgent (the fresh process that re-reads the grants and
        //    serves), then exit. Because a double-clicked .app is its own TCC responsible
        //    process, the LaunchAgent it installs inherits the same grant identity.
        if glass_macos::accessibility_granted() && glass_macos::screen_recording_granted() {
            let exe = std::env::current_exe()?;
            if let Some((label, instruction)) =
                crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
            {
                // The job loaded but isn't serving yet (e.g. a port clash). This double-clicked
                // onboarder has no dialog surface, so surface the actionable instruction to
                // stderr (captured in the app's unified/Console log) rather than exit silently
                // implying success. `label` names the LaunchAgent job.
                eprintln!(
                    "glass onboarding: LaunchAgent {label} installed but isn't serving yet: \
                     {instruction}"
                );
            }
            return Ok(());
        }

        // C. At least one grant is missing — show the checklist so the user can grant each and
        //    re-check. `on_recheck` is assembled here, not inside `grant_row_widgets`, because
        //    only the onboarder's recheck should relaunch-and-exit(0) (see `relaunch`); the
        //    menu-bar self-onboard fallback reuses the row widgets but wires its own
        //    `on_recheck` (`restart_launch_agent()`) since it runs inside the already-serving
        //    process and must not exit it.
        let app = crate::setup::app_bundle_path(&std::env::current_exe().unwrap_or_default());
        let actions = ChecklistActions {
            rows: grant_row_widgets(),
            on_recheck: Box::new(move || relaunch(&app)),
        };
        run_checklist(actions).map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = addr;
        anyhow::bail!("onboarding is macOS-only")
    }
}

/// The permission rows to show, as `(label, granted)` pairs in display order, from this launch's
/// TCC snapshot. Accessibility leads Screen Recording — mirroring the request order in
/// [`grant_row_widgets`], since a Screen Recording grant can raise macOS's "Quit & Reopen"
/// prompt and keeping AX first isolates that interruption to the end. Pure (no AppKit / TCC /
/// IO), so the label set and ordering are unit-tested on Linux while [`grant_row_widgets`] pairs
/// each row with its `request_*`/pane closure. Gated macOS+test like its sole non-test caller so
/// a plain non-test Linux/Windows build doesn't warn it dead.
#[cfg(any(target_os = "macos", test))]
fn grant_rows(accessibility: bool, screen_recording: bool) -> [(&'static str, bool); 2] {
    [
        ("Accessibility", accessibility),
        ("Screen Recording", screen_recording),
    ]
}

/// Build the checklist's row widgets from this launch's TCC grant snapshot: one [`GrantRow`]
/// per permission, each carrying its live snapshot and the "Open Settings" closure that
/// requests the grant and opens its System Settings pane. Factored out of [`run`] so the
/// menu-bar self-onboard fallback (Task 4) can reuse the *identical* row wiring.
///
/// Only the row widgets are shared, not a whole [`ChecklistActions`] — `on_recheck` is
/// necessarily per-caller: the onboarder's recheck relaunches a fresh process and `exit(0)`s
/// (see [`relaunch`]), which would kill the menu-bar app's already-serving process, so that
/// fallback wires its own `on_recheck` (`restart_launch_agent()`) instead. Callers assemble
/// their own [`ChecklistActions`] around these rows.
#[cfg(target_os = "macos")]
pub(crate) fn grant_row_widgets() -> Vec<GrantRow> {
    let [(ax_label, ax_granted), (sr_label, sr_granted)] = grant_rows(
        glass_macos::accessibility_granted(),
        glass_macos::screen_recording_granted(),
    );

    vec![
        GrantRow {
            label: ax_label,
            granted: ax_granted,
            // "Open Settings": add GlassMcp.app to the Accessibility pane + raise the
            // first-time TCC prompt, then deterministically open the pane so the button
            // always lands the user there (a later click, once macOS no longer re-prompts,
            // still opens Settings).
            on_open_settings: Box::new(|| {
                glass_macos::request_accessibility();
                let _ = glass_macos::open_pane(glass_macos::accessibility_pane_url());
            }),
        },
        GrantRow {
            label: sr_label,
            granted: sr_granted,
            on_open_settings: Box::new(|| {
                glass_macos::request_screen_recording();
                let _ = glass_macos::open_pane(glass_macos::screen_recording_pane_url());
            }),
        },
    ]
}

/// Relaunch GlassMcp.app as a NEW process (so it re-reads the per-process-cached TCC snapshot),
/// then exit this instance. `open -n <app>` asks LaunchServices for a fresh instance — a
/// same-process `exec()` would keep the stale TCC snapshot. Exiting after spawning is the clean
/// hand-off: the fresh instance re-enters [`run`] and, once both grants read granted, installs
/// the LaunchAgent (outcome B).
///
/// Only exits on a confirmed-successful spawn. If `open` fails or exits non-zero, the checklist
/// window is already gone by the time this runs, so silently exiting here would strand the user
/// with no relaunched app and no window — report the error to stderr and return instead, which
/// leaves the checklist window open (this is `on_recheck: Box<dyn Fn()>`, so returning is a
/// no-op back to the AppKit run loop) for the user to retry.
#[cfg(target_os = "macos")]
fn relaunch(app_bundle_path: &str) {
    match std::process::Command::new("/usr/bin/open")
        .arg("-n")
        .arg(app_bundle_path)
        .status()
    {
        Ok(status) if status.success() => std::process::exit(0),
        Ok(status) => eprintln!(
            "glass: relaunch (open -n {app_bundle_path}) exited {status}; leaving the checklist \
             open"
        ),
        Err(e) => eprintln!(
            "glass: relaunch (open -n {app_bundle_path}) failed: {e}; leaving the checklist open"
        ),
    }
}

/// Copy `text` to the general pasteboard via `pbcopy` — no AppKit/NSPasteboard dependency needed
/// in this headless binary. `pub(crate)` so the menu-bar app's "Copy endpoint" item
/// (`crate::menubar`) reuses this exact helper rather than rolling its own `pbcopy` shell-out.
/// Gated to `network` + macOS to match that sole caller (`menubar` is itself
/// `#[cfg(feature = "network")]`): the checklist onboarder hands off no endpoint, so without the
/// `network` feature a plain macOS build would have no caller and flag this dead.
#[cfg(all(target_os = "macos", feature = "network"))]
pub(crate) fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("Stdio::piped guarantees a stdin handle")
        .write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_rows_orders_accessibility_before_screen_recording() {
        let rows = grant_rows(false, false);
        assert_eq!(rows[0].0, "Accessibility");
        assert_eq!(rows[1].0, "Screen Recording");
    }
}
