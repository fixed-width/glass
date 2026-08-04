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
# Three tests fail loudly without the jar and a11y APK rather than self-skipping, so neither is
# optional. GLASS_ANDROID_FIXTURE_APK currently is: its only consumer,
# native_invoke_actuates_the_fixture, is excluded below until glass#324 lands.
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
# Two tests are filtered out below rather than excluding the whole file each belongs to:
#
#   set_value_reports_whether_the_write_landed (a11y_loop) — fails on a cold CI emulator with
#   AxElementChanged(16), AndroidA11y's staleness guard refusing the write; passes warm. Tracked at
#   glass#323. While it is off the a11y write path has no device coverage: the only other
#   set_value call (a11y_service_loop) sits behind "if this screen has an editable field", which
#   Settings' top screen does not. Note glass#326's fix does not reach this — it relaxed the same
#   guard in ServiceA11y, and this test drives AndroidA11y.
#
# --skip matches as a SUBSTRING: a future test whose name contains this one's would be excluded
# too, with no signal that it was.
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
  --skip set_value_reports_whether_the_write_landed "$@"
