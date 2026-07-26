//! Recovery for an X11 toplevel the compositor never surfaced.
//!
//! Under load, an app's X11 window can end up mapped in Xwayland's X server yet absent from
//! sway's tree: the window is real and drawable, but no wlroots view was ever created for it,
//! so it is missing from `list_windows` — and if it is the app's only window, `start_app` waits
//! for a window that never arrives and times out on a healthy app. Redrawing does not fix it;
//! the window stays lost for the life of the session.
//!
//! glass owns both halves of that session (it spawns sway, which spawns Xwayland), so it can
//! see the discrepancy the app cannot: compare the X server's mapped toplevels against the
//! windows sway reports, and re-map the ones sway never saw. A fresh map request restarts the
//! handshake that was lost. Each window is re-mapped at most once and only after it has been
//! missing on two checks in a row — a window that is merely mid-handshake looks exactly like a
//! lost one — and whether it then arrives is not guaranteed, so a launch that still gives up
//! reports what glass saw rather than a bare timeout.
//!
//! The comparison is only sound because rootless Xwayland does not reparent its clients: the
//! ids sway reports for its Xwayland views and the root's children in the X server are the same
//! namespace. If that ever stopped holding, every window would read as lost — which is the other
//! reason for the once-per-window bound.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use glass_core::{GlassError, Result};
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::{ConnectionExt, MapState, WindowClass};
use x11rb::rust_connection::RustConnection;

/// How long the compositor is given to publish a re-mapped window's view before the window list
/// is read again. Paid in full on any enumeration that re-mapped something, and only then.
pub const REMAP_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a session may go looking for lost windows, and so also how long a window must have
/// been missing before it is re-mapped.
///
/// Two costs sit behind it. Until a connection is established, each check scans `/proc` for the
/// session's Xwayland — which a native Wayland session, that can never need any of this, would
/// otherwise pay on every window enumeration. Afterwards each check is a round trip per X window.
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(750);

/// A session's state for recovering lost toplevels: which session it is, the X connection once
/// one exists, what has been tried, what was missing last time, and when the last check ran.
pub struct Recovery {
    /// The session's private runtime directory — what identifies its own Xwayland among any
    /// other X servers on the machine. Held rather than passed per call, so a cached connection
    /// and the directory it was found from cannot come from different sessions.
    runtime_dir: PathBuf,
    probe: Option<XProbe>,
    /// Windows a cross-check has already re-mapped — recorded before the attempt, so a re-map
    /// that itself fails is not retried either.
    attempted: HashSet<u32>,
    /// Windows the previous check found mapped in X and absent from the compositor. A window has
    /// to be missing twice running before it is re-mapped: during startup the compositor is
    /// still publishing views, and one that is merely in flight looks exactly like a lost one.
    missing_before: HashSet<u32>,
    /// Windows re-mapped that still had not reached the compositor at the next check. Counted so
    /// a launch that times out can say the recovery ran and did not take, instead of reporting a
    /// bare timeout for an app whose window glass could see all along.
    unrecovered: usize,
    last_checked: Option<std::time::Instant>,
    /// Whether a failure to reach the X side has already been reported. Checks repeat for the
    /// life of the session, and one unreachable X server should read as one warning, not as a
    /// line every interval. Cleared with the connection, so a later failure of a different kind
    /// still gets said once.
    warned: bool,
}

impl Recovery {
    pub fn new(runtime_dir: &Path) -> Recovery {
        Recovery {
            runtime_dir: runtime_dir.to_path_buf(),
            probe: None,
            attempted: HashSet::new(),
            missing_before: HashSet::new(),
            unrecovered: 0,
            last_checked: None,
            warned: false,
        }
    }

    /// How many windows glass re-mapped that never reached the compositor afterwards. A launch
    /// that fails to find a window uses this to say what it saw rather than reporting a bare
    /// timeout.
    pub fn unrecovered(&self) -> usize {
        self.unrecovered
    }

    /// Make the next call check rather than fall inside the interval. Used when a session hands
    /// off to its caller: the first `list_windows` after `start_app` must cross-check, not skip
    /// because startup already looked.
    pub fn rearm(&mut self) {
        self.last_checked = None;
    }

    /// Whether a cross-check is due. The first one always is: a window lost during startup is
    /// lost by the time anything asks for the window list.
    fn due(&self, now: std::time::Instant) -> bool {
        self.last_checked
            .is_none_or(|last| now.duration_since(last) >= CHECK_INTERVAL)
    }

    /// Report a failure to reach the session's X side once. The cross-check repeats for the life
    /// of the session, so an X server that stays unreachable would otherwise repeat its warning
    /// every interval — drowning out whatever the app itself is saying.
    fn warn_once(&mut self, message: String) {
        if !self.warned {
            self.warned = true;
            eprintln!("{message}");
        }
    }

    /// Re-map the app's mapped X11 toplevels that the compositor has no view for, and report how
    /// many — but at most once per [`CHECK_INTERVAL`], because finding the session's Xwayland
    /// costs a `/proc` walk and this sits on the window-enumeration path.
    ///
    /// Best effort: a session with no Xwayland (a native Wayland app), an X server that will not
    /// answer, or a check that is not due yet all leave the caller exactly as it was. The
    /// throttle lives here rather than at the call sites so a caller cannot spend the walk by
    /// forgetting to ask first.
    ///
    /// `in_compositor` is the X11 window id of each window sway currently reports (native
    /// Wayland views have none and are simply absent from it).
    pub fn recover_if_due(&mut self, now: std::time::Instant, in_compositor: &[u32]) -> usize {
        if !self.due(now) {
            return 0;
        }
        self.last_checked = Some(now);
        let probe = match self.probe.take() {
            Some(p) => p,
            None => {
                // No Xwayland holding this session's runtime directory: a native Wayland app,
                // which never takes the path this recovers. Nothing to do, nothing to warn about.
                let Some(display) = session_display(&self.runtime_dir) else {
                    return 0;
                };
                match XProbe::connect(&display) {
                    Ok(p) => p,
                    Err(e) => {
                        self.warn_once(format!(
                            "glass: could not inspect the session's Xwayland display: {e}"
                        ));
                        return 0;
                    }
                }
            }
        };
        let mapped = match probe.mapped_toplevels() {
            Ok(m) => m,
            Err(e) => {
                // The connection is dropped rather than kept (it was taken out of `self` above):
                // an Xwayland that went away leaves a socket that answers nothing, and keeping it
                // would turn one dead connection into recovery being off for the rest of the
                // session. The warning is re-armed with it, so the next failure still gets said.
                self.warned = false;
                self.warn_once(format!(
                    "glass: could not read the session's Xwayland windows: {e}"
                ));
                return 0;
            }
        };
        // Forget windows the X server no longer has. They cannot come back — an X id is only
        // reused after the window is destroyed — and a window that is still lost is still in
        // `mapped`, so this prunes without ever un-suppressing one.
        self.attempted.retain(|w| mapped.contains(w));
        // A window re-mapped by an earlier check that is still missing did not come back. Count
        // it once, so a launch that gives up can say the recovery ran and did not take.
        let missing: HashSet<u32> = mapped
            .iter()
            .copied()
            .filter(|w| !in_compositor.contains(w))
            .collect();
        self.unrecovered = missing.intersection(&self.attempted).count();
        let mut count = 0;
        for win in confirmed_lost(
            &lost_toplevels(&mapped, in_compositor, &self.attempted),
            &self.missing_before,
        ) {
            self.attempted.insert(win);
            match probe.remap(win) {
                Ok(()) => count += 1,
                // The unmap half may already have applied, which would leave a window the user
                // can see hidden — so this is reported as a failure of glass's own machinery,
                // not as a best-effort miss.
                Err(e) => eprintln!(
                    "glass: could not re-map Xwayland window {win:#x}: {e}. The window may now \
                     be hidden; restart the app to get it back."
                ),
            }
        }
        self.missing_before = missing;
        self.probe = Some(probe);
        if count > 0 {
            // Said out loud because it changed the app's windows, not just glass's reading of
            // them: someone comparing the two deserves to know one needed recovering.
            eprintln!(
                "glass: {count} window(s) the app mapped never reached the compositor; \
                 re-mapped them"
            );
        }
        count
    }
}

/// An X11 connection to the session's own Xwayland, used to see the windows the app really has
/// and to re-map the ones the compositor lost.
pub struct XProbe {
    conn: RustConnection,
    root: u32,
}

impl XProbe {
    /// Connect to the Xwayland server serving `display` (e.g. `":1"`).
    pub fn connect(display: &str) -> Result<XProbe> {
        let (conn, screen) = x11rb::connect(Some(display))
            .map_err(|e| GlassError::Backend(format!("connect Xwayland display {display}: {e}")))?;
        let root = conn.setup().roots[screen].root;
        Ok(XProbe { conn, root })
    }

    /// Every mapped toplevel on this display the compositor owes a view — the ones it already
    /// shows as well as any it lost. Which of them are missing is decided against sway's list.
    pub fn mapped_toplevels(&self) -> Result<Vec<u32>> {
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(|e| GlassError::Backend(format!("query Xwayland tree: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("query Xwayland tree reply: {e}")))?;
        let mut out = Vec::new();
        for win in tree.children {
            let attrs = match self
                .conn
                .get_window_attributes(win)
                .map_err(|e| GlassError::Backend(format!("read Xwayland window {win:#x}: {e}")))?
                .reply()
            {
                Ok(a) => a,
                // The one failure that is not a failure: a window destroyed between the tree read
                // and this one no longer needs a view. Anything else — a dead connection above
                // all — must not read as "this window is fine", because a scan that quietly
                // returns fewer windows reads as "nothing was lost".
                Err(ReplyError::X11Error(e)) if e.error_kind == ErrorKind::Window => continue,
                Err(e) => {
                    return Err(GlassError::Backend(format!(
                        "read Xwayland window {win:#x}: {e}"
                    )));
                }
            };
            if WindowFacts::from_attrs(&attrs).owed_a_view() {
                out.push(win);
            }
        }
        Ok(out)
    }

    /// Ask the X server to map `win` again, restarting the map handshake the compositor missed.
    ///
    /// The window is unmapped first: a map request for an already-mapped window is a no-op, so
    /// only the unmap/map pair produces the fresh MapNotify the compositor acts on. The app sees
    /// an `UnmapNotify` then a `MapNotify` for a window it did not touch — glass drives apps as a
    /// black box, and this is the one place it asks the X server to move one.
    ///
    /// The map is retried once: between the two requests the window is unmapped, so giving up
    /// there would leave a window the user could see hidden by glass.
    pub fn remap(&self, win: u32) -> Result<()> {
        self.conn
            .unmap_window(win)
            .map_err(|e| GlassError::Backend(format!("unmap Xwayland window {win:#x}: {e}")))?
            .check()
            .map_err(|e| GlassError::Backend(format!("unmap Xwayland window {win:#x}: {e}")))?;
        match self.map(win) {
            Ok(()) => Ok(()),
            Err(first) => self.map(win).map_err(|_| first),
        }
    }

    fn map(&self, win: u32) -> Result<()> {
        self.conn
            .map_window(win)
            .map_err(|e| GlassError::Backend(format!("re-map Xwayland window {win:#x}: {e}")))?
            .check()
            .map_err(|e| GlassError::Backend(format!("re-map Xwayland window {win:#x}: {e}")))
    }
}

/// The display served by *this session's* Xwayland, or `None` when the session has no X11 side
/// (a native Wayland app — sway only starts Xwayland once an X11 client connects).
///
/// The session's Xwayland is found by its private runtime directory rather than by reading
/// `DISPLAY` out of the app's environment, and that difference is a safety property, not a
/// detail. glass strips `DISPLAY` from the environment it gives the compositor, but `spec.env`
/// is applied afterwards and may put one back — a launch carrying `DISPLAY=:0` would otherwise
/// point this at the user's own desktop, where every window is "missing from sway's tree" and
/// would be re-mapped. Only the compositor's own Xwayland inherits the runtime directory glass
/// created for the session, so matching on it cannot reach an X server glass does not own.
///
/// The process itself has to be found by scanning: sway reparents Xwayland out of its own
/// process tree (its parent becomes init), so walking sway's descendants never finds it.
fn session_display(runtime_dir: &Path) -> Option<String> {
    xwayland_pids().into_iter().find_map(|pid| {
        let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        if !serves_session(&environ, runtime_dir) {
            return None;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        display_from_cmdline(&cmdline)
    })
}

/// Every Xwayland process on the machine. The session's own is picked out of these by its
/// runtime directory; a host with none simply has no X11 side to recover.
fn xwayland_pids() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| is_xwayland(*pid))
        .collect()
}

/// Whether a process's environment (`/proc/<pid>/environ`: NUL-separated `KEY=VALUE`) says it
/// belongs to the session glass created `runtime_dir` for.
fn serves_session(environ: &[u8], runtime_dir: &Path) -> bool {
    environ
        .split(|b| *b == 0)
        .filter_map(|var| std::str::from_utf8(var).ok())
        .find_map(|var| var.strip_prefix("XDG_RUNTIME_DIR="))
        .is_some_and(|dir| Path::new(dir) == runtime_dir)
}

/// The display an Xwayland process serves, from its `/proc/<pid>/cmdline` (NUL-separated argv).
/// Xwayland takes the display as a positional `:N` argument, so the first token of that shape is
/// the display.
fn display_from_cmdline(cmdline: &[u8]) -> Option<String> {
    cmdline
        .split(|b| *b == 0)
        .filter_map(|arg| std::str::from_utf8(arg).ok())
        .find(|arg| is_display_arg(arg))
        .map(str::to_owned)
}

/// Whether `pid` is an Xwayland process, read from `/proc/<pid>/comm`. Also used by teardown,
/// which must not wait on the compositor's own plumbing the way it waits on the app.
pub fn is_xwayland(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "Xwayland")
}

/// `:0`, `:1`, … — a bare display token. Rejects a screen-qualified (`:0.0`) or host-qualified
/// (`host:0`) form, neither of which Xwayland is started with.
fn is_display_arg(arg: &str) -> bool {
    arg.strip_prefix(':')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What the X server says about one window, reduced to what decides whether the compositor
/// owes it a view.
#[derive(Debug, Clone, Copy)]
pub struct WindowFacts {
    /// Mapped and on screen (X's `MapState::VIEWABLE`).
    pub viewable: bool,
    /// Set by clients for menus, tooltips and drag icons: windows a window manager never
    /// manages, and that sway is therefore right to keep out of its window list.
    pub override_redirect: bool,
    /// `InputOutput` rather than `InputOnly` — an `InputOnly` window has no pixels, so
    /// Xwayland never gives it a surface.
    pub drawable: bool,
}

impl WindowFacts {
    /// Reduce one window's X attributes to the facts the decision turns on, so what `viewable`
    /// and `drawable` mean in X terms is stated once, here, rather than at the call site.
    fn from_attrs(attrs: &x11rb::protocol::xproto::GetWindowAttributesReply) -> WindowFacts {
        WindowFacts {
            viewable: attrs.map_state == MapState::VIEWABLE,
            override_redirect: attrs.override_redirect,
            drawable: attrs.class != WindowClass::INPUT_ONLY,
        }
    }

    /// Whether the compositor owes this window a view — i.e. whether its absence from sway's
    /// tree is a loss rather than the correct outcome.
    fn owed_a_view(&self) -> bool {
        self.viewable && !self.override_redirect && self.drawable
    }
}

/// The mapped X11 toplevels the compositor has no view for — the candidates to re-map.
///
/// `already_tried` holds every window a previous pass attempted, successful or not: one attempt
/// per window, so a window the compositor is deliberately not showing cannot turn into an
/// endless map loop. A window that stays lost after its attempt is left out of the window list
/// and counted, so a launch that gives up can say the recovery ran and did not take.
fn lost_toplevels(mapped: &[u32], in_compositor: &[u32], already_tried: &HashSet<u32>) -> Vec<u32> {
    let known: HashSet<u32> = in_compositor.iter().copied().collect();
    mapped
        .iter()
        .copied()
        .filter(|w| !known.contains(w) && !already_tried.contains(w))
        .collect()
}

/// Of the windows missing now, the ones that were also missing at the previous check.
///
/// A window is only re-mapped after being missing twice running. A window mapped in X but not
/// yet in the compositor's tree is the *normal* state mid-handshake — the X window is mapped
/// when the app shows it, and sway only creates a view once the surface is associated and has
/// committed a buffer — so a single sighting cannot tell a lost window from an arriving one, and
/// re-mapping an arriving window disturbs it and spends its one attempt.
fn confirmed_lost(missing_now: &[u32], missing_before: &HashSet<u32>) -> Vec<u32> {
    missing_now
        .iter()
        .copied()
        .filter(|w| missing_before.contains(w))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toplevel() -> WindowFacts {
        WindowFacts {
            viewable: true,
            override_redirect: false,
            drawable: true,
        }
    }

    #[test]
    fn a_viewable_app_window_counts_as_a_toplevel() {
        assert!(toplevel().owed_a_view());
    }

    /// An unmapped window is one the app withdrew; the compositor is right not to show it.
    #[test]
    fn an_unmapped_window_is_not_a_toplevel() {
        let w = WindowFacts {
            viewable: false,
            ..toplevel()
        };
        assert!(!w.owed_a_view());
    }

    /// Menus, tooltips and drag icons set override-redirect: the window manager never manages
    /// them, so their absence from sway's window list is correct, not a loss.
    #[test]
    fn an_override_redirect_window_is_not_a_toplevel() {
        let w = WindowFacts {
            override_redirect: true,
            ..toplevel()
        };
        assert!(!w.owed_a_view());
    }

    /// An InputOnly window has no pixels to show; Xwayland never gives it a surface.
    #[test]
    fn an_input_only_window_is_not_a_toplevel() {
        let w = WindowFacts {
            drawable: false,
            ..toplevel()
        };
        assert!(!w.owed_a_view());
    }

    /// The first enumeration of a session must cross-check: a window lost during startup is
    /// already lost by then, and waiting out an interval would report it missing once first.
    #[test]
    fn the_first_check_of_a_session_is_due_immediately() {
        let r = Recovery::new(Path::new("/tmp/glass-wl.ab"));
        assert!(r.due(std::time::Instant::now()));
    }

    /// Finding the session's Xwayland means scanning `/proc`; at enumeration rates that has to be
    /// throttled, or a native Wayland session pays the scan on every call.
    #[test]
    fn a_check_is_not_due_again_until_the_interval_passes() {
        let mut r = Recovery::new(Path::new("/tmp/glass-wl.ab"));
        let now = std::time::Instant::now();
        assert_eq!(r.recover_if_due(now, &[]), 0);
        assert!(!r.due(now + CHECK_INTERVAL / 2));
        assert!(r.due(now + CHECK_INTERVAL));
    }

    /// A session handing off to its caller re-arms, so the caller's first enumeration cross-checks
    /// instead of falling inside the interval startup already spent.
    #[test]
    fn rearming_makes_the_next_check_due_again() {
        let mut r = Recovery::new(Path::new("/tmp/glass-wl.ab"));
        let now = std::time::Instant::now();
        assert_eq!(r.recover_if_due(now, &[]), 0);
        assert!(!r.due(now));
        r.rearm();
        assert!(r.due(now));
    }

    /// A session whose runtime directory no Xwayland holds is a native Wayland app: no work, no
    /// warning, and the check is still recorded so the scan is not repeated every call.
    #[test]
    fn a_session_with_no_xwayland_recovers_nothing_and_says_nothing() {
        let mut r = Recovery::new(Path::new("/tmp/glass-wl.no-such-session"));
        assert_eq!(r.recover_if_due(std::time::Instant::now(), &[]), 0);
        assert!(!r.warned, "a session with no X11 side is not a failure");
        assert_eq!(r.unrecovered(), 0);
    }

    /// The session's own Xwayland is the one the compositor started, so it inherited the private
    /// runtime directory glass gave the compositor. That is what tells it apart from any other X
    /// server on the machine — including the user's own desktop.
    #[test]
    fn an_xwayland_holding_the_sessions_runtime_dir_is_the_sessions_own() {
        let environ = b"HOME=/tmp/x\0XDG_RUNTIME_DIR=/tmp/glass-wl.ab\0WAYLAND_DISPLAY=wayland-1\0";
        assert!(serves_session(environ, Path::new("/tmp/glass-wl.ab")));
    }

    /// The decisive case: a caller may override `DISPLAY` in the launch environment, pointing the
    /// app at the user's real desktop. Re-mapping windows there would disrupt applications glass
    /// never launched, so an X server outside this session must not be adopted.
    #[test]
    fn an_x_server_outside_the_session_is_not_the_sessions_own() {
        let environ = b"HOME=/home/u\0XDG_RUNTIME_DIR=/run/user/1000\0DISPLAY=:0\0";
        assert!(!serves_session(environ, Path::new("/tmp/glass-wl.ab")));
    }

    #[test]
    fn an_environment_without_a_runtime_dir_is_not_the_sessions_own() {
        assert!(!serves_session(
            b"HOME=/tmp/x\0",
            Path::new("/tmp/glass-wl.ab")
        ));
    }

    #[test]
    fn reads_the_display_from_an_xwayland_command_line() {
        let cmdline = b"Xwayland\0:1\0-rootless\0-core\0-listenfd\09\0";
        assert_eq!(display_from_cmdline(cmdline).as_deref(), Some(":1"));
    }

    /// Flags and their values are skipped, including a value that is itself display-shaped —
    /// Xwayland's real command line puts the display first, and taking a flag's value instead
    /// would send the probe to another server.
    #[test]
    fn ignores_arguments_that_are_not_a_display() {
        let cmdline = b"Xwayland\0-listenfd\09\0-displayfd\0\x37\0:12\0";
        assert_eq!(display_from_cmdline(cmdline).as_deref(), Some(":12"));
    }

    #[test]
    fn reports_no_display_when_the_command_line_has_none() {
        let cmdline = b"Xwayland\0-rootless\0-terminate\0";
        assert_eq!(display_from_cmdline(cmdline), None);
    }

    /// A window is re-mapped only after being missing on two checks in a row. During startup the
    /// compositor is still publishing views, so a window that is merely *in flight* looks exactly
    /// like a lost one — and re-mapping it would disturb a window that was arriving anyway and
    /// spend its one attempt.
    #[test]
    fn a_window_missing_only_once_is_not_yet_confirmed_lost() {
        assert!(confirmed_lost(&[0x400002], &HashSet::new()).is_empty());
    }

    #[test]
    fn a_window_missing_on_two_consecutive_checks_is_confirmed_lost() {
        let before = HashSet::from([0x400002]);
        assert_eq!(confirmed_lost(&[0x400002], &before), vec![0x400002]);
    }

    /// A window that arrived between the two checks is gone from the second reading, so it is
    /// never re-mapped — the compositor was simply slower than one interval.
    #[test]
    fn a_window_that_arrived_between_checks_is_not_confirmed_lost() {
        let before = HashSet::from([0x400002]);
        assert!(confirmed_lost(&[], &before).is_empty());
    }

    #[test]
    fn a_mapped_window_the_compositor_never_surfaced_is_lost() {
        let lost = lost_toplevels(&[0x400000, 0x400002], &[0x400000], &HashSet::new());
        assert_eq!(lost, vec![0x400002]);
    }

    #[test]
    fn a_window_the_compositor_already_shows_is_not_lost() {
        let lost = lost_toplevels(&[0x400000], &[0x400000], &HashSet::new());
        assert!(lost.is_empty());
    }

    /// One re-map per window: a window that stays missing after its re-map must not be re-mapped
    /// on every later call.
    #[test]
    fn a_window_already_remapped_once_is_not_remapped_again() {
        let tried = HashSet::from([0x400002]);
        let lost = lost_toplevels(&[0x400000, 0x400002], &[0x400000], &tried);
        assert!(lost.is_empty());
    }
}
