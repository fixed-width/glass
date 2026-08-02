#!/usr/bin/env bash
# Measure whether X11 races the launch geometry the same way macOS does (#263): 20 cold
# launches under Xvfb, printed disagreement rate. On-demand contributor tooling (the test is
# #[ignore]d) — NOT part of the per-PR gate, so scripts/test-x11.sh does not run it.
#
#   scripts/x11-geometry-settle-measurement.sh
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test -p glass-testapp --test x11_geometry_settle_measurement -- \
    --ignored --nocapture --test-threads=1 "$@"
