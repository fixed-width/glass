use glass_core::{GlassError, Result};

/// Wayland globals the backend requires. Missing any means the compositor is
/// not a suitable wlroots-class compositor (no capture/input) — a hard error.
/// Window enumeration/selection is via sway IPC, not a wayland global.
const REQUIRED_GLOBALS: &[&str] = &[
    "wl_shm",
    "wl_output",
    "zwlr_screencopy_manager_v1",
    "zwlr_virtual_pointer_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
];

/// Error if any required global is not advertised by the compositor.
pub fn verify_globals(advertised: &[&str]) -> Result<()> {
    for req in REQUIRED_GLOBALS {
        if !advertised.contains(req) {
            return Err(GlassError::Backend(format!(
                "compositor does not export required global {req}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_when_all_required_present() {
        let all = [
            "wl_compositor",
            "wl_shm",
            "wl_output",
            "zwlr_screencopy_manager_v1",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
        ];
        assert!(verify_globals(&all).is_ok());
    }

    #[test]
    fn errors_naming_the_missing_global() {
        let missing_capture = [
            "wl_shm",
            "wl_output",
            "zwlr_virtual_pointer_manager_v1",
            "zwp_virtual_keyboard_manager_v1",
        ];
        let err = verify_globals(&missing_capture).unwrap_err();
        assert!(matches!(err, GlassError::Backend(_)));
        assert!(err.to_string().contains("zwlr_screencopy_manager_v1"));
    }
}
