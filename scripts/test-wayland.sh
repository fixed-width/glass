#!/usr/bin/env bash
# Run the #[ignore]d Wayland integration tests (tests/wayland.rs and the ignore-regions MCP
# e2e in tests/wayland_ignore_regions_e2e.rs). Skips cleanly if no glass-discoverable sway
# >=1.12 is present (build+install via https://github.com/fixed-width/sway-build).
#
# NOTE: the sandbox_* tests in tests/wayland.rs require 'bubblewrap' to be installed
# (sudo apt-get install -y bubblewrap on Debian/Ubuntu) AND unprivileged user namespaces
# enabled. Ubuntu 24.04 restricts them via AppArmor
# (kernel.apparmor_restrict_unprivileged_userns=1) — the CI workflow re-enables them.
# The GLASS_SANDBOX env var controls containment for glass-mcp-launched apps generally
# (off / default / strict); it has no effect on integration tests, which set their
# sandbox level explicitly in the AppSpec.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/have-sway.sh
if ! have_sway; then
    echo "no glass-discoverable sway >=1.12; build+install via https://github.com/fixed-width/sway-build. Skipping Wayland tests."
    exit 0
fi
# --test-threads=1: each test spawns its own sway (and Xwayland); serialize
# so concurrent compositors don't contend for the display.
marker="$(mktemp)"
status=0
cargo test -p glass-testapp --test wayland --test wayland_ignore_regions_e2e -- --ignored --test-threads=1 "$@" || status=$?

# A session's runtime dir is dropped by the same teardown that reaps its compositor, so one left
# behind is a teardown that never finished — a live sway still holding the app and the session's
# a11y bus. Newer than the marker only, so an earlier run's residue is not this run's failure.
# Checked here rather than in a test because the tests pass while it happens (glass#415).
leaked=$(find /tmp -maxdepth 1 -name 'glass-wl.*' -newer "$marker" 2>/dev/null || true)
rm -f "$marker"
if [ -n "$leaked" ]; then
    echo "FAIL: the suite leaked $(printf '%s\n' "$leaked" | wc -l) compositor session(s):" >&2
    printf '%s\n' "$leaked" >&2
    echo "Each is a live sway; the test that started it never stopped its session." >&2
    status=1
fi
exit "$status"
