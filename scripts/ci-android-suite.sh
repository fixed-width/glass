#!/usr/bin/env bash
# Run the Android device suite, always saving its output into ./diagnostics/test-output.txt, and
# on failure also collect on-device diagnostics there. Exists as its own repo script rather than
# inline in the workflow's
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
# diagnostics/test-output.txt captures the suite's own stdout/stderr: which test failed and why,
# not just what the device was doing (the diagnostics below) — that text otherwise exists only in
# the ephemeral job log. Captured unconditionally rather than only on failure: `tee` still streams
# it live to the job log, and the upload step is already `if: failure()`, so a passing run costs
# nothing either way.
#
# logcat is cleared before the run and dumped bounded (-t 5000) after: cleared so the failure
# window isn't buried under whatever the AVD logged before this suite started, bounded so a run
# that fails early doesn't wait on (or get truncated by) an oversized buffer.
#
# The screenshot is named screen-after-teardown.png, not screen.png, because that is genuinely
# what it is: scripts/test-android.sh has already returned by the time this fires, so whatever app
# the failing test was mid-interaction with may have already been torn down — for a test that
# stops its own app under test, the capture shows the launcher and proves nothing about the
# failure. It is kept anyway because it is the fastest available signal for whole-device failures
# (the emulator never booted, a black screen, a stuck ANR/crash dialog, sitting on the lock
# screen) — exactly the cases where logcat is largest and least focused. The name states when it
# was taken so it can't be mistaken for a frame at the moment of failure.
#
#   ./scripts/ci-android-suite.sh [extra cargo-test filters]
#
# Not -e: a failing test suite is the expected path this script exists to handle, not a bug in
# the script — it must survive scripts/test-android.sh's failure long enough to collect
# diagnostics and still exit with the real code afterward. Piping through `tee` below normally
# replaces $? with tee's own status; PIPESTATUS[0] is used instead so rc is the test command's
# exit code even if tee itself fails (pipefail alone would not guarantee that).
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
