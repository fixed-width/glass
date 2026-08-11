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
# logcat streams to a file for the whole run instead of being dumped at teardown, which cannot
# reach back to the failure. Measured on an API 34 pixel_6 AVD: still churning after a snapshot
# restore — the package scans and Phenotype config of glass#331 — it logs 1235 lines/s, falling to
# ~1 line/s once that settles. In that churn `logcat -d -t 5000` reaches back four seconds and the
# 2 MiB ring buffer thirteen, against the 86s between glass#338's failure and its teardown. A whole
# run costs tens of MB, which the upload compresses. Nothing is filtered: which tag explains the
# next failure is not knowable in advance.
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

mkdir -p diagnostics

# Cleared, then opened before the suite starts, so this file's first line is the run's own first
# line. threadtime for the pid/tid a bare timestamp doesn't carry.
"$adb" logcat -c
"$adb" logcat -v threadtime > diagnostics/logcat.txt 2>&1 &
logcat_pid=$!
# The action tears the emulator down the moment its step ends, so a writer left running meets that
# teardown rather than this kill.
trap 'kill "$logcat_pid" 2>/dev/null' EXIT

# `boot_completed` is not readiness: the action restores a cached AVD snapshot and reports boot
# the moment the property flips, while package scans and GMS's Phenotype config commits keep
# force-stopping apps for another ~15s — including `com.google.android.settings.intelligence`,
# which the search-screen tests drive. Starting there made the leg red ~25% of runs (glass#331).
#
# The signal is this file's own growth rate, so it costs no adb call and needs no message to be
# matched. Measured on an idle API 34 AVD from `boot_completed`, lines per 5s:
#
#   +0s 4561   +5s 5127   +10s 1132   +15s 787   +20s 229   +25s 14   +30s 50 …
#
# No window before +25s is under 100 and the first that is falls 16x, so the threshold sits in a
# wide gap rather than on a slope. Two consecutive quiet windows, because GMS goes on producing
# bursts (+60s, +100s, +150s here).
#
# The budget is a CEILING, not a wait: a device that settles in 30s pays 30s whatever it is set to,
# so it is sized for "this device is broken" rather than "this device is slow". Ten reruns of the
# same CI job settled at 85 90 90 95 95 95 95 105 105 110s, which is 71-92% of the 120s this
# replaces — one slow run from giving up and starting the suite anyway.
#
# Do NOT re-size this from a local run: the same measurement on this project's dev box was 25-30s,
# 3-4x faster than the CI snapshot restore it has to cover.
#
# It never fails the run: a device that stays busy is a slow one, and the suite is what decides
# that. Both outcomes are logged, or a gate that stopped waiting would read like one that worked.
settle_window=5
settle_quiet=100
settle_needed=2
settle_budget=300

settled=0
quiet=0
waited=0
while [ "$waited" -lt "$settle_budget" ]; do
  before=$(wc -l < diagnostics/logcat.txt 2>/dev/null || echo 0)
  sleep "$settle_window"
  waited=$((waited + settle_window))
  grew=$(( $(wc -l < diagnostics/logcat.txt 2>/dev/null || echo 0) - before ))
  if [ "$grew" -lt "$settle_quiet" ]; then
    quiet=$((quiet + 1))
    if [ "$quiet" -ge "$settle_needed" ]; then
      settled=1
      break
    fi
  else
    quiet=0
  fi
done
if [ "$settled" -eq 1 ]; then
  echo "ci-android-suite: device settled after ${waited}s (last window ${grew} lines)"
else
  echo "ci-android-suite: device still busy after ${settle_budget}s (last window ${grew} lines); \
running the suite anyway" >&2
fi

./scripts/test-android.sh "$@" 2>&1 | tee diagnostics/test-output.txt
rc=${PIPESTATUS[0]}

# The workflow builds these before booting anything, so a `Compiling` line here means the
# pre-build missed a target and this run competed with a compiler — the failure mode of
# glass#439.
if grep -q '^ *Compiling ' diagnostics/test-output.txt; then
  echo "ci-android-suite: the suite compiled with the device up — the pre-build did not cover \
every target, so this run's load is not what the settle gate measured" >&2
fi

if [ "$rc" -ne 0 ]; then
  "$adb" shell dumpsys window        > diagnostics/dumpsys-window.txt 2>&1
  "$adb" shell dumpsys activity top  > diagnostics/dumpsys-top.txt    2>&1
  "$adb" exec-out screencap -p       > diagnostics/screen-after-teardown.png 2>/dev/null
fi

# Stopped last: the capture then covers the collection above, and that collection is also the
# drain. A line still in flight when the kill lands is lost — measured, and host-side line
# buffering does not change it. A writer that died on its own leaves a short file that reads as a
# complete one.
if ! kill -0 "$logcat_pid" 2>/dev/null; then
  echo "ci-android-suite: the logcat writer exited before the suite did; this capture ends early" \
    | tee -a diagnostics/logcat.txt >&2
fi
kill "$logcat_pid" 2>/dev/null
wait "$logcat_pid" 2>/dev/null
trap - EXIT

exit "$rc"
