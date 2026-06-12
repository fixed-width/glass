#!/usr/bin/env bash
# Run the #[ignore]d glass-a11y-linux integration suite. The tests launch with
# a11y:true, so glass spawns its OWN isolated session bus + AT-SPI registry (in a
# private XDG_RUNTIME_DIR) — no external dbus-run-session or manual at-spi-bus-launcher
# is needed. We still gate on the binaries glass *uses* being installed, and skip
# (exit 0) when any prerequisite is missing, mirroring scripts/test-wayland.sh's
# skip-without-sway behavior.
set -euo pipefail
cd "$(dirname "$0")/.."   # -> rust/

launcher=""
for c in /usr/libexec/at-spi-bus-launcher \
         /usr/lib/at-spi2-core/at-spi-bus-launcher \
         /usr/lib/at-spi2/at-spi-bus-launcher \
         /usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher; do
    [ -x "$c" ] && launcher="$c" && break
done

if ! command -v dbus-daemon >/dev/null 2>&1 \
   || [ -z "$launcher" ] \
   || ! command -v Xvfb >/dev/null 2>&1 \
   || ! command -v python3 >/dev/null 2>&1 \
   || ! python3 -c 'import gi; gi.require_version("Gtk", "4.0")' >/dev/null 2>&1; then
    echo "test-a11y: prerequisites missing — glass needs dbus-daemon, at-spi-bus-launcher,"
    echo "           Xvfb, and python3 with GTK4 GI (apt install at-spi2-core"
    echo "           gir1.2-gtk-4.0 python3-gi xvfb dbus-bin). Skipping."
    exit 0
fi

TEST_FILTER="${1:-}"
# --test-threads=1: tests share glass's AT-SPI bus per process; parallel launches
# cause bus instability (fixtures disconnecting race with new connections).
exec cargo test -p glass-a11y-linux --test integration -- --ignored --test-threads=1 "$TEST_FILTER"
