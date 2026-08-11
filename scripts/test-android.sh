#!/usr/bin/env bash
# Run the Android device suite: the #[ignore]d tests across 7 of the 9 files in
# crates/glass-android/tests/, plus the session-level click leg in
# crates/glass-mcp/tests/android_session_loop.rs. Requires an already-booted emulator — this
# script does not start one (scripts/test-android-lifecycle.sh exercises glass booting its own).
#
#   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
#     GLASS_ANDROID_AGENT_JAR=/path/to/glass-agent.jar \
#     GLASS_ANDROID_A11Y_APK=/path/to/glass-a11y.apk \
#     GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
#     ./scripts/test-android.sh
#
# `--build-only` compiles the suite's test binaries and stops, touching no device — CI runs it
# before any emulator exists, because a build beside a booted AVD ANRs the device (glass#439).
#
# Every test needing the jar, the a11y APK or the fixture APK fails loudly without it rather than
# self-skipping, so all three are required.
#
# --test-threads=1 is required: every test drives the one attached device, so in parallel each
# tears down the app another is mid-interaction with.
#
# --no-fail-fast is required: without it cargo stops at the first failing --test binary, so a
# scheduled run would report one failure per night instead of the night's full list.
#
# Two files are deliberately absent from the target list below:
#
#   role_probe   — prints a widget-class histogram for a human and needs GLASS_A11Y_PROBE_APPS to
#                  name app components; a green run carries no information.
#   managed_avd  — requires NO emulator running, so it cannot share a run with tests that need a
#                  booted device. scripts/test-android-lifecycle.sh runs it instead.
#
# Nothing is filtered out. If a test ever has to be, use --skip and say why here — it matches as a
# SUBSTRING, so a future test whose name contains the excluded one's would go too, unannounced.
set -euo pipefail
cd "$(dirname "$0")/.."
. scripts/lib/device-residue.sh

build_only=0
if [ "${1:-}" = "--build-only" ]; then
  build_only=1
  shift
fi

# One list for both the build and the run — a target added to only one gets compiled during the
# suite, with the device up.
android_targets=(
  --test a11y_loop
  --test a11y_service_loop
  --test agent_loop
  --test doctor_loop
  --test input_loop
  --test see_loop
  --test window_loop
)

# Before the device checks: this runs with no emulator up.
if [ "$build_only" -eq 1 ]; then
  cargo test --no-run -p glass-android "${android_targets[@]}"
  cargo test --no-run -p glass-mcp --test android_session_loop
  exit 0
fi

adb="${GLASS_ADB:-adb}"
if ! before="$(device_snapshot "$adb")"; then
    echo "cannot read the device (GLASS_ADB=${GLASS_ADB:-<unset>}, \
GLASS_ANDROID_SERIAL=${GLASS_ANDROID_SERIAL:-<unset>}); is one attached?" >&2
    exit 1
fi
assert_clean_baseline "$before" || exit 1

# Two invocations, not one `-p glass-android -p glass-mcp`: a `--test` filter applies to every
# `-p` package, so neither package matches the other's targets and the run exits 0 having tested
# nothing.
rc=0

cargo test --no-fail-fast -p glass-android "${android_targets[@]}" \
  -- --ignored --test-threads=1 "$@" || rc=$?

# The session-level click leg (glass#287): it lives in glass-mcp because that crate owns the
# factory deciding which accessibility reader a session gets, and it costs a glass-mcp build the
# rest of the suite does not need.
cargo test --no-fail-fast -p glass-mcp \
  --test android_session_loop \
  -- --ignored --test-threads=1 "$@" || rc=$?

# After the suite, not per test: the check costs a settle wait, and a test that leaks hands the
# device to every test after it anyway.
residue=0
report_device_residue "$adb" "$before" || residue=1
# Only when the suite itself passed, so a leak does not overwrite cargo's 101 with a 1 and hide
# which kind of failure this was.
if [ "$rc" -eq 0 ]; then
  rc="$residue"
fi

exit "$rc"
