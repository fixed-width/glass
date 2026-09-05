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
# The direct test runs are `cargo nextest run`; the harness scripts below still run libtest.
# Each test gets its own process, so one that aborts takes down itself rather than its whole
# binary — and every binary's tests share one pool, so tests from different crates overlap for
# the first time. Nothing here serializes on a fixed port, path or display; a test that needs to
# must say so itself. nextest runs no doctests, and the workspace has none.
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
cd "$(dirname "$0")/.." || exit 1 # the workspace root
. scripts/lib/have-sway.sh

if ! cargo llvm-cov --version >/dev/null 2>&1; then
    echo "coverage: cargo-llvm-cov is not installed." >&2
    echo "          install it with:  cargo install cargo-llvm-cov" >&2
    echo "          and the llvm-tools component:  rustup component add llvm-tools-preview" >&2
    exit 1
fi

if ! cargo nextest --version >/dev/null 2>&1; then
    echo "coverage: cargo-nextest is not installed." >&2
    echo "          install it with:  cargo install cargo-nextest --locked" >&2
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

# Export the instrumentation env so every test run below — the direct nextest runs and the
# `cargo test`s the harnesses run — writes .profraw into the shared coverage target dir.
# `LLVM_PROFILE_FILE` carries `%p`; without it nextest's process-per-test clobbers one file.
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
        # Here-string, not `printf | grep -q`: under `pipefail` the early-exiting grep SIGPIPEs
        # the printf, and the pipeline reports 141 on a MATCH. Only bites past a pipe buffer.
        if grep -qi 'skipping' <<<"$out"; then
            skipped+=("$label")
        else
            ran+=("$label")
        fi
    else
        failed+=("$label")
    fi
}

# A direct nextest run rather than a harness. Its output must NOT be grepped for the skip
# sentinel the way `classify_suite` does — a self-skipping test prints "skipping" too, and one
# of them would file the whole run as skipped.
#
# Exit status decides it. `--no-fail-fast` keeps one failing test from costing the rest of the
# run its coverage; `--no-tests=fail` exits 4 where libtest exits 0 on a run that selected
# nothing and records it as coverage. Per invocation, so it catches a `-p` leg that ran nothing,
# not one silent crate inside `--workspace`.
#
# It hardcodes the runner, hence the name: a leg needing libtest — a doctest one, which nextest
# cannot run — needs its own helper, not this one with different arguments.
run_nextest() { # label nextest-args...
    local label="$1" out rc
    shift
    out=$(cargo nextest run --no-fail-fast --no-tests=fail "$@" 2>&1)
    rc=$?
    if [ "$rc" -eq 0 ]; then
        ran+=("$label")
    else
        # Exit 4 ran nothing, so the summary below must not credit it with coverage.
        [ "$rc" -eq 4 ] && label="$label (ran no tests)"
        failed+=("$label")
        tail -n 30 <<<"$out"
    fi
}

# Always: the workspace unit tests (and the always-on integration tests).
echo "coverage: running workspace unit tests…"
run_nextest unit --workspace

if [ "$unit_only" -eq 0 ]; then
    # Each harness exits 0 and self-skips when its prerequisites are missing, so we
    # distinguish "ran" from "skipped" by probing the prerequisite ourselves.
    # test-x11.sh does NOT self-skip (it errors if Xvfb is absent), so probe first.
    echo "coverage: running X11 integration suite (needs Xvfb; sandbox_* need bubblewrap)…"
    if command -v Xvfb >/dev/null 2>&1; then
        # The workspace run above skipped glass-x11's `#[ignore]`d display tests — half the
        # crate's coverage.
        run_nextest x11-unit -p glass-x11 --run-ignored=all
        classify_suite x11 ./scripts/test-x11.sh
    else
        skipped+=("x11-unit (no Xvfb)" "x11 (no Xvfb)")
    fi

    echo "coverage: running Wayland integration suite (needs sway >=1.12)…"
    if have_sway; then
        run_nextest wayland-unit -p glass-wayland --run-ignored=all
    else
        skipped+=("wayland-unit (no sway)")
    fi
    classify_suite wayland ./scripts/test-wayland.sh

    echo "coverage: running AT-SPI integration suite (needs dbus/at-spi2/GTK3/GTK4)…"
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
[ ${#failed[@]} -gt 0 ] && echo "coverage: suites that failed (a failing suite still contributes its coverage): ${failed[*]}"
echo "coverage: NOTE glass-windows Win32 FFI is cfg(windows) — not measured on Linux (on-box only)."
# Exit non-zero only if the unit suite itself failed to build/run; integration
# flakiness shouldn't fail a coverage report.
case " ${failed[*]} " in *" unit "*) exit 1 ;; esac
exit 0
