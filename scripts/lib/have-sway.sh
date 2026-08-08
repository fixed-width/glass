# shellcheck shell=bash
# Whether a sway the Wayland backend will actually accept is installed.
#
# One copy because two disagreed: `command -v sway` alone is wrong in both directions. The
# documented install puts the bundle in $XDG_DATA_HOME and NOT on PATH, and Ubuntu ships 1.9,
# which `resolve_sway` rejects. Keep this in step with `resolve_sway` in
# crates/glass-wayland/src/platform.rs.
have_sway() {
    local bundle="${XDG_DATA_HOME:-$HOME/.local/share}/glass/sway/bin/sway"
    [ -x "$bundle" ] && return 0
    [ -n "${GLASS_SWAY:-}" ] && [ -x "${GLASS_SWAY:-}" ] && return 0
    command -v sway >/dev/null 2>&1 &&
        sway --version 2>/dev/null | grep -qE 'version 1\.(1[2-9]|[2-9][0-9])'
}
