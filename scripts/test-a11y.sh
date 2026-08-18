#!/usr/bin/env bash
# Run the #[ignore]d glass-a11y-linux integration suite. The tests launch with
# a11y:true, so glass spawns its OWN isolated session bus + AT-SPI registry (in a
# private XDG_RUNTIME_DIR) — no external dbus-run-session or manual at-spi-bus-launcher
# is needed. We still gate on the binaries glass *uses* being installed, and skip
# (exit 0) when any prerequisite is missing, mirroring scripts/test-wayland.sh's
# skip-without-sway behavior.
set -euo pipefail
cd "$(dirname "$0")/.."   # -> rust/

. scripts/lib/have-atspi.sh
. scripts/lib/have-sway.sh

if ! command -v dbus-daemon >/dev/null 2>&1 \
   || ! have_atspi \
   || ! command -v Xvfb >/dev/null 2>&1 \
   || ! command -v python3 >/dev/null 2>&1 \
   || ! python3 -c 'import gi; gi.require_version("Gtk", "4.0")' >/dev/null 2>&1; then
    echo "test-a11y: prerequisites missing — glass needs dbus-daemon, at-spi-bus-launcher,"
    echo "           Xvfb, and python3 with GTK4 GI (apt install at-spi2-core"
    echo "           gir1.2-gtk-4.0 python3-gi xvfb dbus-bin). Skipping."
    exit 0
fi

TEST_FILTER="${1:-}"

# Three of these tests drive the app under Wayland, so they need the same sway >=1.12
# scripts/test-wayland.sh does. Named, not silently dropped: a run that says nothing about them
# reads as "the suite passed" when a quarter of the launch paths never ran.
SKIP_ARGS=()
if ! have_sway; then
    echo "test-a11y: no glass-discoverable sway >=1.12 — skipping the wayland_a11y_* tests."
    SKIP_ARGS=(--skip wayland_a11y)
fi

# Hermetic isolation: run the whole suite under a throwaway XDG_RUNTIME_DIR. AT-SPI derives its
# socket dir from XDG_RUNTIME_DIR, so any at-spi-bus-launcher these tests spawn — through glass's
# PrivateBus (which already overrides it) OR any other path — writes to this throwaway, NEVER the
# operator's real /run/user/UID/at-spi. Unconditional belt-and-suspenders: the desktop's own
# accessibility bus (md-viewer, etc.) cannot be wedged by a test run, regardless of code paths.
A11Y_TEST_RUNTIME="$(mktemp -d "${TMPDIR:-/tmp}/glass-a11y-test-rt.XXXXXX")"
chmod 700 "$A11Y_TEST_RUNTIME"
trap 'rm -rf "$A11Y_TEST_RUNTIME"' EXIT
export XDG_RUNTIME_DIR="$A11Y_TEST_RUNTIME"

# --test-threads=1: tests share glass's AT-SPI bus per process; parallel launches
# cause bus instability (fixtures disconnecting race with new connections).
#
# glass-dbus-linux's PrivateBus tests (private bus + at-spi bring-up, including the
# org.a11y.Status.ScreenReaderEnabled advertisement that accesskit-based apps gate on) run
# under the same throwaway XDG_RUNTIME_DIR isolation.
cargo test -p glass-dbus-linux --lib -- --ignored --test-threads=1 "$TEST_FILTER"
cargo test -p glass-a11y-linux --test integration -- --ignored --test-threads=1 "${SKIP_ARGS[@]}" "$TEST_FILTER"
# Its own binary, so the environment it mutates is nothing else's (see the file's header).
cargo test -p glass-a11y-linux --test launcher_override -- --ignored "$TEST_FILTER"
# Likewise, and --test-threads=1 so the environment it repoints stays this test's alone.
cargo test -p glass-a11y-linux --test host_bus_unresponsive -- --ignored --test-threads=1 "$TEST_FILTER"
