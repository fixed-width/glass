#!/usr/bin/env bash
# Boot the AVD the device suite runs against, and return only once it is genuinely ready.
#
#   ./scripts/ci-android-boot.sh <avd-name>
#
# Not android-emulator-runner: it waits on `sys.boot_completed` alone and then sends
# `input keyevent 82` unconditionally, and input dispatched before the launcher has a focused
# window ANRs the launcher — whose dialog then takes focus for the rest of the run (glass#441).
#
# Readiness here is the focused window rather than a property. The keyguard is dismissed through
# `wm`, which needs no dispatch and so cannot time out.
set -euo pipefail
cd "$(dirname "$0")/.."

avd="${1:?usage: ci-android-boot.sh <avd-name>}"
adb="${GLASS_ADB:-adb}"
emulator="${ANDROID_SDK_ROOT:?ANDROID_SDK_ROOT is not set}/emulator/emulator"
serial="${GLASS_ANDROID_SERIAL:-emulator-5554}"
boot_budget="${GLASS_CI_BOOT_BUDGET:-600}"

# An ANR record reports the guest's load average, not the host's, so without both numbers a
# run's CPU pressure cannot be told from a busy host.
echo "--- capacity ---"
echo "host: $(nproc) cpu(s), $(free -g | awk '/^Mem:/ {print $2}')G ram"
grep -E '^(hw\.cpu\.ncore|hw\.ramSize)' "${ANDROID_AVD_HOME:-$HOME/.android/avd}/$avd.avd/config.ini" 2>/dev/null \
  || echo "(no cpu/ram overrides in the AVD config)"

# -no-snapshot, not -no-snapshot-save: a restored snapshot resumes mid-churn, the state the
# settle gate has to wait out. A cold boot costs about a minute.
#
# To a file, not this script's stdout: a backgrounded child inherits stdout, so an emulator
# writing into a pipe holds it open after this script exits, and whatever reads that pipe waits
# for the emulator.
mkdir -p diagnostics
echo "--- launching $avd (log: diagnostics/emulator.log) ---"
"$emulator" -avd "$avd" -port "${serial##*-}" \
  -no-snapshot -no-window -noaudio -no-boot-anim \
  -gpu swiftshader_indirect -camera-back none > diagnostics/emulator.log 2>&1 &
emulator_pid=$!
# `emu kill` first: killing the launcher PID can leave the qemu process it forked behind, and a
# stray emulator makes the next run attach to a device nobody booted.
trap '"$adb" -s "$serial" emu kill >/dev/null 2>&1; kill "$emulator_pid" 2>/dev/null' EXIT

# One budget covers appearing, booting and reaching a drivable window — the three are one wait
# from the caller's side. `wait-for-device` is not used: it has no bound, so an emulator that
# never registers would hold the job open to its own timeout instead of failing here.
deadline=$((SECONDS + boot_budget))
while ! "$adb" -s "$serial" get-state > /dev/null 2>&1; do
  [ "$SECONDS" -lt "$deadline" ] || { echo "::error::$serial never appeared within ${boot_budget}s" >&2; exit 1; }
  sleep 2
done

while [ "$($adb -s "$serial" shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" != "1" ]; do
  [ "$SECONDS" -lt "$deadline" ] || { echo "::error::sys.boot_completed never set within ${boot_budget}s" >&2; exit 1; }
  sleep 2
done
echo "boot_completed after $((SECONDS))s"

# A keyguard is dismissed and re-checked, not assumed gone: dismiss-keyguard returns before the
# window does.
dismissed=0
while :; do
  [ "$SECONDS" -lt "$deadline" ] || { echo "::error::no drivable focused window within ${boot_budget}s; last focus: ${focus:-<none>}" >&2; exit 1; }
  focus="$($adb -s "$serial" shell dumpsys window displays 2>/dev/null | grep 'mCurrentFocus=' || true)"
  case "$focus" in
    *"Application Not Responding"*)
      echo "::error::an app stopped responding before the suite began: ${focus#*mCurrentFocus=}" >&2
      exit 1
      ;;
    *Keyguard*|*StatusBar*)
      [ "$dismissed" -eq 1 ] || { echo "dismissing the keyguard"; $adb -s "$serial" shell wm dismiss-keyguard; dismissed=1; }
      ;;
    # FallbackHome is the placeholder Android shows while the user is still locked. Focus alone
    # would call it ready — the same too-early answer as sys.boot_completed.
    *FallbackHome*|*mCurrentFocus=null*|"")
      ;;
    *)
      echo "focused window after $((SECONDS))s: ${focus#*mCurrentFocus=}"
      break
      ;;
  esac
  sleep 2
done

# The suite's timing assertions are written against a device with animations off.
for scale in window_animation_scale transition_animation_scale animator_duration_scale; do
  "$adb" -s "$serial" shell settings put global "$scale" 0.0
done

trap - EXIT
echo "--- ready ---"
