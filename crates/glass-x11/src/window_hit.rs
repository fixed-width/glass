//! X-server hit testing across window boundaries, including input-shaped covers.

use glass_core::{Deadline, GlassError, Result};
use x11rb::protocol::xproto::{ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

pub(crate) fn occluded(
    conn: &RustConnection,
    root: Window,
    target: Window,
    point: (i32, i32),
    deadline: Deadline,
) -> Result<bool> {
    let range_error = || GlassError::Backend("pointer point exceeds X11 coordinate range".into());
    let x = i16::try_from(point.0).map_err(|_| range_error())?;
    let y = i16::try_from(point.1).map_err(|_| range_error())?;
    let mut child = target;
    for _ in 0..64 {
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed(
                "X11 pointer window probe",
            ));
        }
        let parent = conn
            .query_tree(child)
            .map_err(protocol_error)?
            .reply()
            .map_err(protocol_error)?
            .parent;
        if parent == x11rb::NONE {
            return Err(GlassError::Backend(
                "active window has no containing X11 window".into(),
            ));
        }
        // TranslateCoordinates returns the topmost mapped child whose bounding and input
        // shapes contain the point. Checking each ancestor also covers reparenting WMs.
        let hit = conn
            .translate_coordinates(target, parent, x, y)
            .map_err(protocol_error)?
            .reply()
            .map_err(protocol_error)?;
        if !hit.same_screen {
            return Err(GlassError::Backend(
                "pointer window probe crossed X11 screens".into(),
            ));
        }
        if hit.child != child {
            return Ok(true);
        }
        if parent == root {
            return Ok(false);
        }
        child = parent;
    }
    Err(GlassError::Backend(
        "X11 window ancestry exceeds 64 levels".into(),
    ))
}

fn protocol_error(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("X11 pointer window probe: {error}"))
}
