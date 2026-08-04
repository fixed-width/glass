#!/usr/bin/env bash
# Run the Android device suite, saving its output to ./diagnostics/test-output.txt and, on
# failure, on-device diagnostics alongside it. Do not inline this back into the workflow's
# `script:` block, for two reasons:
#
# 1. reactivecircus/android-emulator-runner splits `script:` on newlines and runs EACH LINE as
#    its own independent `sh -c`, stopping the moment one exits non-zero (src/script-parser.ts
#    and the exec.exec loop in src/main.ts, at the pin this repo uses). A multi-line
#    run-then-collect block there is silently fragmented and the collection never runs.
# 2. Collection cannot be a later `if: failure()` step either: the action tears the emulator down
#    the instant its own step ends, leaving no adb device to talk to.
#
# diagnostics/test-output.txt carries which test failed and why; that text otherwise exists only
# in the ephemeral job log.
#
# logcat is cleared before the run so the failure window isn't buried under what the AVD logged
# beforehand, and dumped bounded (-t 5000) so an early failure isn't truncated by an oversized
# buffer.
#
# screen-after-teardown.png is named for when it is taken: test-android.sh has already returned,
# so a test that stops its own app leaves only the launcher on screen. Kept because it is the
# fastest signal for whole-device failures — never booted, black screen, stuck ANR dialog, lock
# screen — where logcat is largest and least focused.
#
#   ./scripts/ci-android-suite.sh [extra cargo-test filters]
#
# Do not add -e: this script must survive test-android.sh's failure to collect diagnostics. Piping
# through `tee` replaces $? with tee's status, so rc comes from PIPESTATUS[0] (pipefail alone
# would not guarantee it).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

# Resolve adb the way the tests do, and fail here if it isn't there. Without -e, an unresolvable
# adb below would not stop anything: each redirect would write "command not found" into a
# diagnostics file, which then uploads and reads as collected evidence.
adb="${GLASS_ADB:-adb}"
if ! command -v "$adb" > /dev/null 2>&1; then
  echo "ci-android-suite: adb not found (GLASS_ADB=${GLASS_ADB:-<unset>})" >&2
  exit 1
fi

"$adb" logcat -c
mkdir -p diagnostics

./scripts/test-android.sh "$@" 2>&1 | tee diagnostics/test-output.txt
rc=${PIPESTATUS[0]}

if [ "$rc" -ne 0 ]; then
  "$adb" logcat -d -t 5000           > diagnostics/logcat.txt         2>&1
  "$adb" shell dumpsys window        > diagnostics/dumpsys-window.txt 2>&1
  "$adb" shell dumpsys activity top  > diagnostics/dumpsys-top.txt    2>&1
  "$adb" exec-out screencap -p       > diagnostics/screen-after-teardown.png 2>/dev/null
fi

exit "$rc"
