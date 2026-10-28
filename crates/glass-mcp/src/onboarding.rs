//! First-run application setup and grant-bearing account onboarding.
//!
//! The normal app preserves an existing shared-desktop setup and offers account setup on first run.
//! The development account's permission helper has an explicit launchd role in the same bundle.

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
use glass_macos::onboarding_window::{ChecklistActions, GrantRow, run_checklist};

/// Open the enrolled menu-bar app, or guide first-time environment setup.
pub fn run(addr: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        match glass_macos::private_session::configured() {
            Ok(true) => {
                #[cfg(feature = "network")]
                {
                    let exe = std::env::current_exe()?;
                    return crate::setup::start_launch_agent(&exe.to_string_lossy(), addr)
                        .map_err(Into::into);
                }
                #[cfg(not(feature = "network"))]
                return crate::macos_environment::show_setup(addr);
            }
            Ok(false) => {}
            Err(_) => return crate::macos_environment::show_setup(addr),
        }
        if !matches!(crate::macos_environment::has_pending_setup(), Ok(false)) {
            return crate::macos_environment::show_setup(addr);
        }
        // A. A healthy agent is already serving with both grants held — nothing to onboard.
        //    Don't re-request grants (would pop dialogs for nothing) or reinstall over a working
        //    agent.
        if crate::setup::fetch_health(addr).is_some_and(|h| h.grants_ready()) {
            return Ok(());
        }

        if glass_macos::accessibility_granted() && glass_macos::screen_recording_granted() {
            let exe = std::env::current_exe()?;
            if let Some((label, instruction)) =
                crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
            {
                eprintln!("glass: {label}: {instruction}");
            }
            return Ok(());
        }

        match glass_macos::setup_window::choose_environment().map_err(anyhow::Error::msg)? {
            glass_macos::setup_window::SetupChoice::SeparateAccount => {
                crate::macos_environment::show_setup(addr)
            }
            glass_macos::setup_window::SetupChoice::Cancel => Ok(()),
            glass_macos::setup_window::SetupChoice::SharedDesktop => {
                let exe = std::env::current_exe()?;
                if let Some((label, instruction)) =
                    crate::setup::install_launch_agent(&exe.to_string_lossy(), addr)?
                {
                    eprintln!("glass: {label}: {instruction}");
                }
                Ok(())
            }
        }
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
/// per permission, each carrying its live snapshot and the "Request…" closure that requests
/// the grant. Shared with the menu-bar self-onboard fallback, which reuses the *identical* row
/// wiring.
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
            on_request: Box::new(glass_macos::request_accessibility),
            on_open_settings: Box::new(|| {
                glass_macos::open_pane(glass_macos::accessibility_pane_url())
                    .map_err(|error| error.to_string())
            }),
        },
        GrantRow {
            label: sr_label,
            granted: sr_granted,
            on_request: Box::new(glass_macos::request_screen_recording),
            on_open_settings: Box::new(|| {
                glass_macos::open_pane(glass_macos::screen_recording_pane_url())
                    .map_err(|error| error.to_string())
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

/// Grant-bearing onboarding in the selected account's real login.
#[cfg(target_os = "macos")]
pub fn run_worker() -> anyhow::Result<()> {
    glass_macos::private_session::attach(true)?;
    if glass_macos::accessibility_granted() && glass_macos::screen_recording_granted() {
        eprintln!(
            "glass: permissions ready; return to your normal account; Start environment can restart this desktop after logout"
        );
        return Ok(());
    }
    let app = crate::setup::app_bundle_path(&std::env::current_exe()?);
    run_checklist(ChecklistActions {
        rows: grant_row_widgets(),
        on_recheck: Box::new(move || relaunch(&app)),
    })
    .map_err(|error| anyhow::anyhow!(error))
}

/// Explicit launchd entry; does not rely on Finder's no-argument launch classification.
#[cfg(target_os = "macos")]
pub fn run_registered_worker(owner: u32, check: bool) -> anyhow::Result<()> {
    use glass_macos::onboarding_window::{ChecklistUpdate, run_checklist_with_poll};
    use std::sync::{Arc, Mutex};

    if check {
        glass_macos::private_session::attach_for(owner, true)?;
    } else {
        while !glass_macos::private_session::try_attach_for_onboarding(owner)?
            || glass_macos::private_session::login_setup_pending()?
        {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
    let facts = crate::macos_setup::Readiness {
        private_session: glass_macos::private_session::require_private().is_ok(),
        login_setup_pending: glass_macos::private_session::login_setup_pending()?,
        accessibility: glass_macos::accessibility_granted(),
        screen_recording: glass_macos::screen_recording_granted(),
        onboarding_running: false,
    };
    if check {
        println!("{}", serde_json::to_string(&facts)?);
        return Ok(());
    }
    if !facts.login_setup_pending && facts.accessibility && facts.screen_recording {
        return Ok(());
    }
    glass_macos::init_main_thread();
    let latest = Arc::new(Mutex::new(None));
    let output = latest.clone();
    let binary = std::env::current_exe()?;
    std::thread::spawn(move || {
        loop {
            let result = fresh_worker_readiness(&binary, owner);
            if let Ok(mut output) = output.lock() {
                *output = Some(result);
            } else {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    });
    run_checklist_with_poll(ChecklistActions {
        rows: grant_row_widgets(),
        on_recheck: Box::new(|| {
            let target = format!("gui/{}/tech.fixedwidth.glass.onboarding", rustix::process::getuid().as_raw());
            if let Err(error) = std::process::Command::new("/bin/launchctl").args(["kickstart", "-k", &target]).spawn() {
                eprintln!("glass: could not restart permission onboarding: {error}");
            }
        }),
    }, Some(Box::new(move || {
        let result = latest.lock().ok()?.take()?;
        Some(match result {
            Ok(facts) => {
                let granted = facts.accessibility && facts.screen_recording;
                ChecklistUpdate {
                    granted: [facts.accessibility, facts.screen_recording],
                    message: if facts.login_setup_pending { "Finish all macOS welcome screens until this account's desktop appears. Then log out and return to your normal account. Start environment in Glass starts this desktop when needed.".into() }
                        else if granted { "Permissions ready. You can log out and return to your normal account. In Glass application setup, choose Start environment if this desktop is stopped, or Continue setup if checking is paused.".into() }
                        else { "Glass permissions can be approved from your normal account in System Settings → Privacy & Security. Existing grants are reused across accounts. Return there after completing macOS welcome screens; you can log out of this account. Glass checks automatically; there is no time limit.".into() },
                    finished: granted && facts.private_session && !facts.login_setup_pending,
                }
            }
            Err(error) => ChecklistUpdate {
                granted: [facts.accessibility, facts.screen_recording],
                message: format!("Could not check permissions: {error}. You can retry with Re-check."),
                finished: false,
            },
        })
    }))).map_err(anyhow::Error::msg)
}

#[cfg(target_os = "macos")]
fn fresh_worker_readiness(
    binary: &std::path::Path,
    owner: u32,
) -> anyhow::Result<crate::macos_setup::Readiness> {
    use anyhow::Context;
    use std::io::Read;
    use std::process::{Command, Stdio};
    let mut child = Command::new(binary)
        .args(["macos-onboarding", &owner.to_string(), "--check"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(status.success(), "fresh permission check failed");
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .context("permission check stdout unavailable")?
                .take(4096)
                .read_to_end(&mut bytes)?;
            return Ok(serde_json::from_slice(&bytes)?);
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("fresh permission check timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
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
