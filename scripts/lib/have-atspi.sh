# shellcheck shell=bash
# Whether this host has an at-spi-bus-launcher glass can spawn.
#
# One copy because two disagreed: both gates hardcoded /usr/lib/x86_64-linux-gnu, so an aarch64
# host with at-spi2-core installed skipped its suite as "prerequisites missing", and neither
# honoured GLASS_ATSPI_LAUNCHER, which the crate consults before anything else. Keep this in step
# with `find_launcher` in crates/glass-dbus-linux/src/lib.rs.
have_atspi() {
    local c
    # An override names the launcher outright and is not searched for among the candidates —
    # glass fails closed on a wrong one rather than quietly spawning a different binary.
    if [ -n "${GLASS_ATSPI_LAUNCHER:-}" ]; then
        [ -f "${GLASS_ATSPI_LAUNCHER}" ] && [ -x "${GLASS_ATSPI_LAUNCHER}" ]
        return
    fi
    # -f as well as -x: a directory is executable to `test` and is not a launch target.
    for c in /usr/libexec/at-spi-bus-launcher \
             /usr/lib/at-spi2-core/at-spi-bus-launcher \
             /usr/lib/at-spi2/at-spi-bus-launcher \
             /usr/lib/*/at-spi2-core/at-spi-bus-launcher; do
        if [ -f "$c" ] && [ -x "$c" ]; then
            return 0
        fi
    done
    return 1
}
