#!/usr/bin/env bash
# Run the #[ignore]d Wayland integration tests. Skips cleanly if no
# glass-discoverable sway >=1.12 is present (build+install via https://github.com/fixed-width/sway-build).
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
SWAY_BUNDLE="${XDG_DATA_HOME:-$HOME/.local/share}/glass/sway/bin/sway"
if [ ! -x "$SWAY_BUNDLE" ] && ! { command -v sway >/dev/null 2>&1 && sway --version 2>/dev/null | grep -qE 'version 1\.(1[2-9]|[2-9][0-9])'; }; then
    echo "no glass-discoverable sway >=1.12; build+install via https://github.com/fixed-width/sway-build. Skipping Wayland tests."
    exit 0
fi
# --test-threads=1: each test spawns its own sway (and Xwayland); serialize
# so concurrent compositors don't contend for the display.
exec cargo test -p glass-testapp --test wayland -- --ignored --test-threads=1 "$@"
