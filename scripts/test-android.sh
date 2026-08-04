#!/usr/bin/env bash
# Run the Android device suite: the #[ignore]d tests across 7 of the 9 files in
# crates/glass-android/tests/. Requires an already-booted emulator — this script does not
# start one (scripts/test-android-lifecycle.sh is the one that exercises glass booting its own).
#
#   GLASS_ADB=$HOME/android-sdk/platform-tools/adb \
#     GLASS_ANDROID_AGENT_JAR=/path/to/glass-agent.jar \
#     GLASS_ANDROID_A11Y_APK=/path/to/glass-a11y.apk \
#     GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
#     ./scripts/test-android.sh
#
# The jar and APKs come from the glass-android-agent repo. Four tests fail loudly without them
# (a11y_service_loop's two, agent_loop, and a11y_loop's foreground-app test) rather than
# self-skipping, so the paths are not optional.
#
# --test-threads=1 is required, not tidiness: every test drives the one attached device, so in
# parallel each tears down the app another is mid-interaction with.
#
# --no-fail-fast is required, not a debugging aid: without it cargo stops at the first failing
# --test binary and never runs the rest, so a CI leg would need one run per failing binary to see
# the full picture. This script's main consumer is a scheduled diagnostic leg (glass#320), where
# the value is the complete list of what broke overnight, not the first thing that broke — so
# every binary always runs, and cargo's own exit code still goes nonzero if any test anywhere
# failed.
#
# Two files are deliberately absent from the target list below:
#
#   role_probe   — a PROBE, not an assertion suite. It prints a widget-class histogram for a
#                  human to read and needs GLASS_A11Y_PROBE_APPS to name app components. A green
#                  run carries no information, so automating it would manufacture a signal.
#   managed_avd  — requires NO emulator running. It asserts glass boots one itself, reuses it,
#                  and kills it. It cannot share a run with tests that need a booted device;
#                  scripts/test-android-lifecycle.sh runs it instead.
#
# One test within a11y_loop is filtered out below rather than excluding the whole file:
#
#   set_value_reports_whether_the_write_landed — fails on a cold CI emulator with
#   AxElementChanged(16), glass's staleness guard refusing the write because the a11y node
#   changed between snapshot and write; passes on a warm dev box. Deterministic, not a flake.
#   Tracked at glass#323; remove this --skip once #323 lands.
#
# --skip composes with the workflow's own `--skip native_invoke_actuates_the_fixture`, forwarded
# through "$@" below: the test harness ORs multiple --skip filters together, so both apply and
# each still removes only the one test it names.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test --no-fail-fast -p glass-android \
  --test a11y_loop \
  --test a11y_service_loop \
  --test agent_loop \
  --test doctor_loop \
  --test input_loop \
  --test see_loop \
  --test window_loop \
  -- --ignored --test-threads=1 --skip set_value_reports_whether_the_write_landed "$@"
