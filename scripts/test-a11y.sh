#!/usr/bin/env bash
# Run the #[ignore]d glass-a11y-linux integration suite under a private session bus
# + AT-SPI registry. Skips (exit 0) when prerequisites are missing, mirroring
# scripts/test-wayland.sh's skip-without-sway behavior.
set -euo pipefail
cd "$(dirname "$0")/.."   # -> rust/

launcher=""
for c in /usr/libexec/at-spi-bus-launcher \
         /usr/lib/at-spi2-core/at-spi-bus-launcher \
         /usr/lib/at-spi2/at-spi-bus-launcher \
         /usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher; do
    [ -x "$c" ] && launcher="$c" && break
done

if ! command -v dbus-run-session >/dev/null 2>&1 \
   || [ -z "$launcher" ] \
   || ! command -v Xvfb >/dev/null 2>&1 \
   || ! command -v python3 >/dev/null 2>&1 \
   || ! python3 -c 'import gi; gi.require_version("Gtk", "4.0")' >/dev/null 2>&1; then
    echo "test-a11y: prerequisites missing — need dbus-run-session, at-spi-bus-launcher,"
    echo "           Xvfb, and python3 with GTK4 GI (apt install at-spi2-core"
    echo "           gir1.2-gtk-4.0 python3-gi xvfb dbus-x11). Skipping."
    exit 0
fi

TEST_FILTER="${1:-}"
exec dbus-run-session -- bash -c '
    set -e
    # Pre-start gnome-keyring on the private session bus so xdg-desktop-portal
    # resolves org.freedesktop.secrets immediately (avoids a ~25 s timeout that
    # delays GTK4 window presentation on a fresh private bus).
    if command -v gnome-keyring-daemon >/dev/null 2>&1; then
        gnome-keyring-daemon --daemonize --components=secrets 2>/dev/null || true
        sleep 0.3
    fi

    # AT-SPI registry for the accessibility reader.
    "'"$launcher"'" &
    launcher_pid=$!
    trap "kill $launcher_pid 2>/dev/null || true" EXIT
    sleep 0.5
    # --test-threads=1: tests share the same AT-SPI bus; parallel launches
    # cause bus instability (fixtures disconnecting race with new connections).
    cargo test -p glass-a11y-linux --test integration -- --ignored --test-threads=1 '"$TEST_FILTER"'
'
