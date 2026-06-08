#!/usr/bin/env bash
# Measure test coverage with cargo-llvm-cov (nightly toolchain, picked up
# automatically via rust-toolchain.toml).
#
# Why this wrapper rather than a bare `cargo llvm-cov`: glass's most security- and
# backend-relevant code is exercised only by the #[ignore]d integration suites
# (X11/Wayland/AT-SPI), which each need a display/bus and are run via their own
# harness scripts. A plain `cargo llvm-cov` runs only the always-on unit tests and
# therefore reports the backends (glass-x11/glass-wayland/glass-a11y-linux
# platform code) as near-0% when they are in fact covered. This script uses
# `cargo llvm-cov show-env` to export the instrumentation environment, then runs
# the unit tests AND each integration harness (each self-skips when its
# prerequisites are absent), and finally combines them into one report.
#
# CAVEAT (always true on Linux): the glass-windows Win32 FFI code (capture, input,
# clipboard, process, util, windows) is cfg(windows) and is NOT compiled or run
# here, so it never appears in this report. Its pure modules (dpi/jobpids/vkmap/
# containment::config/…) DO appear. Windows coverage is an on-box concern.
#
# Usage:
#   scripts/coverage.sh                 # unit + all available integration suites, summary
#   scripts/coverage.sh --unit-only     # unit tests only (fast; undercounts backends)
#   scripts/coverage.sh --html          # also write an HTML report and print its path
#   scripts/coverage.sh --open          # write HTML and open it
#   scripts/coverage.sh --lcov          # also write target/llvm-cov/glass.lcov (for CI upload)
# Extra args after a `--` are passed to the final `cargo llvm-cov report`.
set -uo pipefail
cd "$(dirname "$0")/.."   # -> rust/

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "coverage: cargo-llvm-cov is not installed." >&2
    echo "          install it with:  cargo install cargo-llvm-cov" >&2
    echo "          and the llvm-tools component:  rustup component add llvm-tools-preview" >&2
    exit 1
fi

unit_only=0
want_html=0
want_open=0
want_lcov=0
report_args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --unit-only) unit_only=1 ;;
        --html)      want_html=1 ;;
        --open)      want_html=1; want_open=1 ;;
        --lcov)      want_lcov=1 ;;
        --)          shift; report_args+=("$@"); break ;;
        *)           echo "coverage: unknown flag '$1'" >&2; exit 2 ;;
    esac
    shift
done

# Export the instrumentation env so every `cargo test` below (incl. the ones the
# integration harnesses run) writes .profraw into the shared coverage target dir.
# shellcheck disable=SC1090
source <(cargo llvm-cov show-env --sh)
cargo llvm-cov clean --workspace

ran=()
skipped=()
failed=()

# Classify a self-skipping harness by exit status and its skip sentinel:
#   exit != 0            -> failed (a real test failure)
#   exit 0 + "Skipping"  -> skipped (prerequisites absent; the harness opted out)
#   exit 0 otherwise     -> ran
# (test-wayland.sh / test-a11y.sh print "Skipping …" and exit 0 when prereqs are
# missing, so a clean opt-out isn't miscounted as a real run.)
classify_suite() {  # label cmd...
    local label="$1"; shift
    local out
    if out=$("$@" 2>&1); then
        if printf '%s\n' "$out" | grep -qi 'skipping'; then
            skipped+=("$label")
        else
            ran+=("$label")
        fi
    else
        failed+=("$label")
    fi
}

# Always: the workspace unit tests (and the always-on integration tests). Keep
# going on failure so a single failing test still yields a coverage report.
echo "coverage: running workspace unit tests…"
if cargo test --workspace --no-fail-fast >/dev/null; then ran+=("unit"); else failed+=("unit"); fi

if [ "$unit_only" -eq 0 ]; then
    # Each harness exits 0 and self-skips when its prerequisites are missing, so we
    # distinguish "ran" from "skipped" by probing the prerequisite ourselves.
    # test-x11.sh does NOT self-skip (it errors if Xvfb is absent), so probe first.
    echo "coverage: running X11 integration suite (needs Xvfb; sandbox_* need bubblewrap)…"
    if command -v Xvfb >/dev/null 2>&1; then
        classify_suite x11 ./scripts/test-x11.sh
    else
        skipped+=("x11 (no Xvfb)")
    fi

    echo "coverage: running Wayland integration suite (needs sway >=1.12)…"
    classify_suite wayland ./scripts/test-wayland.sh

    echo "coverage: running AT-SPI integration suite (needs dbus/at-spi2/GTK4)…"
    classify_suite a11y ./scripts/test-a11y.sh
fi

# Combine everything captured above into one report.
echo
echo "coverage: combined report"
cargo llvm-cov report --summary-only "${report_args[@]}"

if [ "$want_lcov" -eq 1 ]; then
    mkdir -p target/llvm-cov   # `report --lcov --output-path` does not create the parent dir
    cargo llvm-cov report --lcov --output-path target/llvm-cov/glass.lcov
    echo "coverage: wrote target/llvm-cov/glass.lcov"
fi
if [ "$want_html" -eq 1 ]; then
    cargo llvm-cov report --html >/dev/null
    echo "coverage: HTML report at target/llvm-cov/html/index.html"
    [ "$want_open" -eq 1 ] && cargo llvm-cov report --html --open >/dev/null
fi

echo
echo "coverage: suites run:     ${ran[*]:-none}"
echo "coverage: suites skipped: ${skipped[*]:-none}"
[ ${#failed[@]} -gt 0 ] && echo "coverage: suites with failing tests (coverage still collected): ${failed[*]}"
echo "coverage: NOTE glass-windows Win32 FFI is cfg(windows) — not measured on Linux (on-box only)."
# Exit non-zero only if the unit suite itself failed to build/run; integration
# flakiness shouldn't fail a coverage report.
case " ${failed[*]} " in *" unit "*) exit 1 ;; esac
exit 0
