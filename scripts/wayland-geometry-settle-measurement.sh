#!/usr/bin/env bash
# Measure whether Wayland races the launch geometry the same way macOS does (#263): 20 cold
# launches under a fresh sway compositor per run, printed disagreement rate. On-demand
# contributor tooling (the test is #[ignore]d) — NOT part of the per-PR gate, so
# scripts/test-wayland.sh does not run it. Skips cleanly if no glass-discoverable sway >=1.12
# is present (build+install via https://github.com/fixed-width/sway-build).
#
#   scripts/wayland-geometry-settle-measurement.sh
set -euo pipefail
cd "$(dirname "$0")/.."
SWAY_BUNDLE="${XDG_DATA_HOME:-$HOME/.local/share}/glass/sway/bin/sway"
if [ ! -x "$SWAY_BUNDLE" ] && ! { command -v sway >/dev/null 2>&1 && sway --version 2>/dev/null | grep -qE 'version 1\.(1[2-9]|[2-9][0-9])'; }; then
    echo "no glass-discoverable sway >=1.12; build+install via https://github.com/fixed-width/sway-build. Skipping."
    exit 0
fi
exec cargo test -p glass-testapp --test wayland_geometry_settle_measurement -- \
    --ignored --nocapture --test-threads=1 "$@"
