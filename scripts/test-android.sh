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
exec cargo test --no-fail-fast -p glass-android \
  --test a11y_loop \
  --test a11y_service_loop \
  --test agent_loop \
  --test doctor_loop \
  --test input_loop \
  --test see_loop \
  --test window_loop \
  -- --ignored --test-threads=1 "$@"
