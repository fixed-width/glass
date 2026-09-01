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
# Use `./scripts/test-wayland.sh artifact` for the native Wayland and Xwayland protected-path cases.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/have-sway.sh
. scripts/lib/session-residue.sh
if ! have_sway; then
    echo "no glass-discoverable sway >=1.12; build+install via https://github.com/fixed-width/sway-build. Skipping Wayland tests."
    exit 0
fi
# --test-threads=1: each test spawns its own sway (and Xwayland); serialize
# so concurrent compositors don't contend for the display.
marker="$(residue_marker)"
trap 'rm -f "$marker"' EXIT
status=0
cargo test -p glass-testapp --test wayland --test wayland_ignore_regions_e2e -- --ignored --test-threads=1 "$@" || status=$?
report_residue "$marker" 'glass-wl.??????' 'glass-a11y-??????' || status=1
exit "$status"
