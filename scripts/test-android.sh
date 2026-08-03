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
# Two files are deliberately absent from the target list below:
#
#   role_probe   — a PROBE, not an assertion suite. It prints a widget-class histogram for a
#                  human to read and needs GLASS_A11Y_PROBE_APPS to name app components. A green
#                  run carries no information, so automating it would manufacture a signal.
#   managed_avd  — requires NO emulator running. It asserts glass boots one itself, reuses it,
#                  and kills it. It cannot share a run with tests that need a booted device;
#                  scripts/test-android-lifecycle.sh runs it instead.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test -p glass-android \
  --test a11y_loop \
  --test a11y_service_loop \
  --test agent_loop \
  --test doctor_loop \
  --test input_loop \
  --test see_loop \
  --test window_loop \
  -- --ignored --test-threads=1 "$@"
