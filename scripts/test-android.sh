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
# The jar and APKs come from the glass-android-agent repo. Three of the tests that run fail
# loudly without them (a11y_service_loop's snapshot test, agent_loop, and a11y_loop's
# foreground-app test) rather than self-skipping, so GLASS_ANDROID_AGENT_JAR and
# GLASS_ANDROID_A11Y_APK are not optional. GLASS_ANDROID_FIXTURE_APK currently is: its only
# consumer, native_invoke_actuates_the_fixture, is excluded below until glass#324 lands.
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
# Two tests are filtered out below rather than excluding the whole file each belongs to:
#
#   set_value_reports_whether_the_write_landed (a11y_loop) — fails on a cold CI emulator with
#   AxElementChanged(16), glass's staleness guard refusing the write because the a11y node
#   changed between snapshot and write; passes on a warm dev box. Deterministic, not a flake.
#   Tracked at glass#323; remove this --skip once #323 lands. While it is off the a11y write path
#   has no device coverage at all: the only other set_value call (a11y_service_loop) is
#   best-effort behind "if this screen has an editable field", which Settings' top screen does not.
#
#   native_invoke_actuates_the_fixture (a11y_service_loop) — fails 4 times in 9 CI runs (~45%) on
#   a cold emulator with Backend("agent: no active window"), at varying sites in
#   a11y_service_loop.rs; passes warm. Tracked at glass#324; remove this --skip once #324 lands.
#
# Multiple --skip flags compose rather than the later one overriding the earlier: the test
# harness ORs them together, so both names above are excluded. Each is matched as a SUBSTRING,
# not an exact name — a future test whose name contains either of these would be excluded too,
# with no signal that it was. (--exact would stop that, but it applies to every filter, including
# the ones a caller passes through "$@".) "$@" below still forwards any further filter on top of
# these two.
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
  -- --ignored --test-threads=1 \
  --skip set_value_reports_whether_the_write_landed \
  --skip native_invoke_actuates_the_fixture "$@"
