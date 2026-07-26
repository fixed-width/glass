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
//! handshake that was lost, and the window appears.

use std::collections::HashSet;

use glass_core::{GlassError, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, MapState, WindowClass};
use x11rb::rust_connection::RustConnection;

/// How long the re-mapped window is given to reach sway's tree. The compositor answers a fresh
/// map request in well under this; it is a bound on the wait, not an expected cost.
pub const REMAP_SETTLE: std::time::Duration = std::time::Duration::from_millis(250);

/// How often a session may go looking for lost windows. Finding the session's Xwayland means
/// reading every process's status, so an untimed check would put a full `/proc` walk in front of
/// every window enumeration — including for native Wayland apps, which can never need this.
pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(750);

/// A session's state for recovering lost toplevels: the X connection once it exists, the windows
/// already re-mapped, and when the next cross-check is due.
#[derive(Default)]
pub struct Recovery {
    probe: Option<XProbe>,
    remapped: HashSet<u32>,
    last_checked: Option<std::time::Instant>,
    /// Whether a failure to reach the X side has already been reported. Checks repeat for the
    /// life of the session, and one unreachable X server should read as one warning, not as a
    /// line every interval.
    warned: bool,
}

impl Recovery {
    pub fn new() -> Recovery {
        Recovery::default()
    }

    /// Whether a cross-check is due. The first one always is: a window lost during startup is
    /// lost by the time anything asks for the window list.
    pub fn due(&self, now: std::time::Instant) -> bool {
        self.last_checked
            .is_none_or(|last| now.duration_since(last) >= CHECK_INTERVAL)
    }

    pub fn mark_checked(&mut self, now: std::time::Instant) {
        self.last_checked = Some(now);
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
    /// many. Best effort: a session with no Xwayland (a native Wayland app), or an X server that
    /// will not answer, leaves the caller exactly as it was.
    ///
    /// `in_compositor` is the X11 window id of each window sway currently reports (native
    /// Wayland views have none and are simply absent from it).
    pub fn recover(&mut self, sway_pid: u32, in_compositor: &[u32]) -> usize {
        self.mark_checked(std::time::Instant::now());
        if self.probe.is_none() {
            // No display in the session at all: a native Wayland app, which never takes the path
            // this recovers. Nothing to do, and nothing to warn about.
            let Some(display) = session_display(sway_pid) else {
                return 0;
            };
            match XProbe::connect(&display) {
                Ok(p) => self.probe = Some(p),
                Err(e) => {
                    self.warn_once(format!(
                        "glass: could not inspect the session's Xwayland display: {e}"
                    ));
                    return 0;
                }
            }
        }
        let Some(probe) = self.probe.as_ref() else {
            return 0;
        };
        let mapped = match probe.mapped_toplevels() {
            Ok(m) => m,
            Err(e) => {
                self.warn_once(format!(
                    "glass: could not read the session's Xwayland windows: {e}"
                ));
                return 0;
            }
        };
        let mut count = 0;
        for win in lost_toplevels(&mapped, in_compositor, &self.remapped) {
            self.remapped.insert(win);
            match probe.remap(win) {
                Ok(()) => count += 1,
                Err(e) => eprintln!("glass: could not re-map Xwayland window {win:#x}: {e}"),
            }
        }
        if count > 0 {
            // Say it out loud: the app did nothing wrong, and someone comparing what the app
            // shows with what glass reports deserves to know a window needed recovering.
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

    /// The app's mapped toplevels: the root's children the compositor owes a view.
    pub fn mapped_toplevels(&self) -> Result<Vec<u32>> {
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(|e| GlassError::Backend(format!("query Xwayland tree: {e}")))?
            .reply()
            .map_err(|e| GlassError::Backend(format!("query Xwayland tree reply: {e}")))?;
        let mut out = Vec::new();
        for win in tree.children {
            // A window can be destroyed between the tree read and the attribute read; that is a
            // window that no longer needs a view, so skip it rather than failing the whole scan.
            let Some(attrs) = self
                .conn
                .get_window_attributes(win)
                .ok()
                .and_then(|c| c.reply().ok())
            else {
                continue;
            };
            let facts = WindowFacts {
                viewable: attrs.map_state == MapState::VIEWABLE,
                override_redirect: attrs.override_redirect,
                drawable: attrs.class != WindowClass::INPUT_ONLY,
            };
            if is_app_toplevel(&facts) {
                out.push(win);
            }
        }
        Ok(out)
    }

    /// Ask the X server to map `win` again, restarting the map handshake the compositor missed.
    ///
    /// The window is unmapped first: a map request for an already-mapped window is a no-op, so
    /// only the unmap/map pair produces the fresh MapNotify the compositor acts on.
    pub fn remap(&self, win: u32) -> Result<()> {
        self.conn
            .unmap_window(win)
            .map_err(|e| GlassError::Backend(format!("unmap Xwayland window {win:#x}: {e}")))?
            .check()
            .map_err(|e| GlassError::Backend(format!("unmap Xwayland window {win:#x}: {e}")))?;
        self.conn
            .map_window(win)
            .map_err(|e| GlassError::Backend(format!("re-map Xwayland window {win:#x}: {e}")))?
            .check()
            .map_err(|e| GlassError::Backend(format!("re-map Xwayland window {win:#x}: {e}")))
    }
}

/// The X display an Xwayland process serves, read from its `/proc/<pid>/cmdline` (NUL-separated
/// argv). Xwayland takes the display as a positional `:N` argument, so the first such token is
/// the display; anything else (a flag, a flag's value) is skipped.
pub fn display_from_cmdline(cmdline: &[u8]) -> Option<String> {
    cmdline
        .split(|b| *b == 0)
        .filter_map(|arg| std::str::from_utf8(arg).ok())
        .find(|arg| is_display_arg(arg))
        .map(str::to_owned)
}

/// The display served by the Xwayland the compositor spawned, or `None` if the session has no
/// X11 side at all (a native Wayland app — sway only starts Xwayland for an X11 client).
///
/// Read from the environment of the processes sway launched, not from Xwayland itself: sway
/// reparents Xwayland out of its own process tree (its parent becomes init), so a tree walk
/// never finds it, while every process sway `exec`s inherits `DISPLAY` naming that server.
/// glass strips `DISPLAY` from the environment it gives sway, so a value found here can only
/// have come from this session's own Xwayland — never from a display glass inherited.
pub fn session_display(sway_pid: u32) -> Option<String> {
    glass_proc_linux::proc_tree_pids(sway_pid)
        .into_iter()
        .find_map(|pid| {
            let environ = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
            display_from_environ(&environ)
        })
}

/// The X display named by a process's environment (`/proc/<pid>/environ`: NUL-separated
/// `KEY=VALUE`).
pub fn display_from_environ(environ: &[u8]) -> Option<String> {
    environ
        .split(|b| *b == 0)
        .filter_map(|var| std::str::from_utf8(var).ok())
        .find_map(|var| var.strip_prefix("DISPLAY="))
        .filter(|display| is_display_arg(display))
        .map(str::to_owned)
}

/// Whether `pid` is the compositor's Xwayland, read from `/proc/<pid>/comm`. Xwayland exits with
/// the compositor and is glass's own plumbing, so teardown must not wait on it the way it waits
/// on the app's own processes.
pub fn is_xwayland(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "Xwayland")
}

/// `:0`, `:1`, … — a display token, as opposed to a flag or a flag's value.
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

/// Whether the compositor owes this window a view — i.e. whether its absence from sway's tree
/// is a loss rather than the correct outcome.
pub fn is_app_toplevel(w: &WindowFacts) -> bool {
    w.viewable && !w.override_redirect && w.drawable
}

/// The mapped X11 toplevels the compositor has no view for — the windows to re-map.
///
/// `already_tried` holds the windows a previous pass re-mapped: a window that stays lost after
/// its re-map is reported rather than re-mapped forever, so a window the compositor is
/// deliberately not showing can't turn into an endless map loop.
pub fn lost_toplevels(
    mapped: &[u32],
    in_compositor: &[u32],
    already_tried: &HashSet<u32>,
) -> Vec<u32> {
    let known: HashSet<u32> = in_compositor.iter().copied().collect();
    mapped
        .iter()
        .copied()
        .filter(|w| !known.contains(w) && !already_tried.contains(w))
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
        assert!(is_app_toplevel(&toplevel()));
    }

    /// An unmapped window is one the app withdrew; the compositor is right not to show it.
    #[test]
    fn an_unmapped_window_is_not_a_toplevel() {
        let w = WindowFacts {
            viewable: false,
            ..toplevel()
        };
        assert!(!is_app_toplevel(&w));
    }

    /// Menus, tooltips and drag icons set override-redirect: the window manager never manages
    /// them, so their absence from sway's window list is correct, not a loss.
    #[test]
    fn an_override_redirect_window_is_not_a_toplevel() {
        let w = WindowFacts {
            override_redirect: true,
            ..toplevel()
        };
        assert!(!is_app_toplevel(&w));
    }

    /// An InputOnly window has no pixels to show; Xwayland never gives it a surface.
    #[test]
    fn an_input_only_window_is_not_a_toplevel() {
        let w = WindowFacts {
            drawable: false,
            ..toplevel()
        };
        assert!(!is_app_toplevel(&w));
    }

    /// The first enumeration of a session must cross-check: a window lost during startup is
    /// already lost by then, and waiting out an interval would report it missing once first.
    #[test]
    fn the_first_check_of_a_session_is_due_immediately() {
        let r = Recovery::new();
        assert!(r.due(std::time::Instant::now()));
    }

    /// Finding the session's Xwayland means reading every process's status; at enumeration rates
    /// that has to be throttled, or a native Wayland session pays a full `/proc` walk per call.
    #[test]
    fn a_check_is_not_due_again_until_the_interval_passes() {
        let mut r = Recovery::new();
        let now = std::time::Instant::now();
        r.mark_checked(now);
        assert!(!r.due(now + CHECK_INTERVAL / 2));
        assert!(r.due(now + CHECK_INTERVAL));
    }

    #[test]
    fn reads_the_display_from_a_process_environment() {
        let environ = b"HOME=/tmp/x\0DISPLAY=:1\0XDG_RUNTIME_DIR=/tmp/glass-wl.ab\0";
        assert_eq!(display_from_environ(environ).as_deref(), Some(":1"));
    }

    /// `WAYLAND_DISPLAY` ends in the same eight characters and names a Wayland socket, not an X
    /// display; matching it would send glass off to connect to nothing.
    #[test]
    fn does_not_mistake_wayland_display_for_the_x_display() {
        let environ = b"WAYLAND_DISPLAY=wayland-1\0HOME=/tmp/x\0";
        assert_eq!(display_from_environ(environ), None);
    }

    #[test]
    fn reads_no_display_from_an_environment_without_one() {
        let environ = b"HOME=/tmp/x\0PATH=/usr/bin\0";
        assert_eq!(display_from_environ(environ), None);
    }

    #[test]
    fn reads_the_display_from_an_xwayland_command_line() {
        let cmdline = b"Xwayland\0:1\0-rootless\0-core\0-listenfd\09\0";
        assert_eq!(display_from_cmdline(cmdline).as_deref(), Some(":1"));
    }

    /// A flag's value can look like anything; only a bare `:N` names the display.
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
