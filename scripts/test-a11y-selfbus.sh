#!/usr/bin/env bash
# Run the capstone test that proves glass spawns its OWN private a11y bus: NO external
# dbus-run-session / at-spi-bus-launcher here. Needs the binaries present (glass starts
# them) plus Xvfb + GTK4 GI. Skips (exit 0) if prereqs are missing.
set -euo pipefail
cd "$(dirname "$0")/.."

launcher=""
for c in /usr/libexec/at-spi-bus-launcher /usr/lib/at-spi2-core/at-spi-bus-launcher \
         /usr/lib/at-spi2/at-spi-bus-launcher /usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher; do
    [ -x "$c" ] && launcher="$c" && break
done
if ! command -v dbus-daemon >/dev/null 2>&1 || [ -z "$launcher" ] \
   || ! command -v Xvfb >/dev/null 2>&1 || ! command -v python3 >/dev/null 2>&1 \
   || ! python3 -c 'import gi; gi.require_version("Gtk", "4.0")' >/dev/null 2>&1; then
    echo "test-a11y-selfbus: prerequisites missing (need dbus-daemon, at-spi-bus-launcher,"
    echo "                   Xvfb, python3 GTK4 GI). Skipping."
    exit 0
fi

# Crucially: NO external a11y/session bus — unset DBUS_SESSION_BUS_ADDRESS so the only
# bus available is the one glass spawns; a regression (no private bus) then fails.
exec env -u DBUS_SESSION_BUS_ADDRESS \
    cargo test -p glass-a11y-linux --test integration -- --ignored --test-threads=1 \
    glass_self_provisions_a11y_bus
