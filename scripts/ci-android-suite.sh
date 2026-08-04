#!/usr/bin/env bash
# Run the Android device suite and, only on failure, collect on-device diagnostics into
# ./diagnostics/. Exists as its own repo script rather than inline in the workflow's
# `script:` block for two reasons:
#
# 1. reactivecircus/android-emulator-runner does not run `script:` as one shell session: it
#    splits the block on newlines and runs EACH LINE as its own independent `sh -c`, and stops
#    the moment one line exits non-zero (src/script-parser.ts + the for-loop over exec.exec in
#    src/main.ts, at the pin this repo uses). A multi-line `run the suite; if it failed, collect
#    diagnostics` block there is silently fragmented: the collection lines are later, unreached
#    iterations of an already-abandoned loop, not later lines of one script. Confirmed against a
#    real CI failure where the diagnostics step logged "No files were found" — the collection
#    commands never ran at all. Putting the whole sequence in one script file gives the workflow
#    a single `script:` line, which is exactly the unit that action's model handles correctly.
#
# 2. Collection can't be a later `if: failure()` step in the workflow either: the emulator-runner
#    action tears the emulator down the instant its own step ends, so a subsequent step has no
#    adb device left to talk to. It has to happen here, inside the same script invocation that
#    ran the tests, while the emulator is still up.
#
# logcat is cleared before the run and dumped bounded (-t 5000) after: cleared so the failure
# window isn't buried under whatever the AVD logged before this suite started, bounded so a run
# that fails early doesn't wait on (or get truncated by) an oversized buffer.
#
#   ./scripts/ci-android-suite.sh --skip native_invoke_actuates_the_fixture
#
# Not -e: a failing test suite is the expected path this script exists to handle, not a bug in
# the script — it must survive scripts/test-android.sh's failure long enough to collect
# diagnostics and still exit with the real code afterward.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

adb logcat -c

./scripts/test-android.sh "$@"
rc=$?

if [ "$rc" -ne 0 ]; then
  mkdir -p diagnostics
  adb logcat -d -t 5000           > diagnostics/logcat.txt         2>&1
  adb shell dumpsys window        > diagnostics/dumpsys-window.txt 2>&1
  adb shell dumpsys activity top  > diagnostics/dumpsys-top.txt    2>&1
  adb exec-out screencap -p       > diagnostics/screen.png         2>/dev/null
fi

exit "$rc"
