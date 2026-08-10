#!/usr/bin/env bash
# Self-test for device-residue.sh, against a stub adb. Needs no device:
#
#   ./scripts/lib/device-residue-test.sh
#
# A leak detector that is never shown to fail is indistinguishable from one that always passes,
# and this one has three ways to go quiet that are invisible on a real run: an unreadable device
# compared against another unreadable device, a clean teardown that normalises `null` to `0`, and
# a baseline that already carries the leak. Each has a case below.
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1
. scripts/lib/device-residue.sh
GLASS_RESIDUE_SETTLE=2
unset GLASS_ANDROID_SERIAL

work=$(mktemp -d "${TMPDIR:-/tmp}/glass-residue-selftest.XXXXXX")
trap 'rm -rf "$work"' EXIT
adb="$work/adb"

# A stub adb driven by $work/state: `services|enabled|forwards`, or `broken`/`silent`.
cat > "$adb" <<'STUB'
#!/usr/bin/env bash
state=$(cat "$(dirname "$0")/state")
[ "$state" = broken ] && { echo "error: no devices/emulators found" >&2; exit 1; }
[ "$state" = silent ] && exit 0
IFS='|' read -r services enabled forwards <<< "$state"
args=("$@"); [ "${args[0]}" = "-s" ] && args=("${args[@]:2}")
case "${args[0]}" in
  forward) [ -n "$forwards" ] && tr ';' '\n' <<< "$forwards" ;;
  shell)   case "${args[4]}" in
             enabled_accessibility_services) echo "$services" ;;
             accessibility_enabled) echo "$enabled" ;;
           esac ;;
esac
exit 0
STUB
chmod +x "$adb"
state() { printf '%s' "$1" > "$work/state"; }

pass=0 fail=0
check() { # label expected actual
    if [ "$2" = "$3" ]; then echo "  ok   $1"; pass=$((pass + 1))
    else echo "  FAIL $1 — expected rc=$2, got rc=$3"; fail=$((fail + 1)); fi
}

echo "a clean run:"
state 'null|0|'
before=$(device_snapshot "$adb"); check "the snapshot reads" 0 $?
report_device_residue "$adb" "$before" > /dev/null 2>&1; check "no change passes" 0 $?

echo "a leak:"
state 'null|0|'
before=$(device_snapshot "$adb")
state "com.fixedwidth.glassa11y/x|1|emulator-5554 tcp:1 localabstract:glass-agent"
out=$(report_device_residue "$adb" "$before" 2>&1); check "it fails" 1 $?
grep -q 'GLASS-RESIDUE: LEAKED' <<< "$out"; check "it prints the greppable marker" 0 $?
grep -q 'glass-agent' <<< "$out"; check "the diff names the forward" 0 $?

echo "an unreadable device:"
state 'null|0|'
before=$(device_snapshot "$adb")
state broken
out=$(report_device_residue "$adb" "$before" 2>&1); check "fails rather than passing" 1 $?
grep -q 'could not read the device' <<< "$out"; check "it says why" 0 $?

echo "adb exiting 0 having printed nothing:"
state silent
device_snapshot "$adb" > /dev/null 2>&1; check "an empty reading is not a snapshot" 1 $?

echo "a teardown that normalised null to 0 (not a leak):"
state 'null|null|'
before=$(device_snapshot "$adb")
state 'null|0|'
report_device_residue "$adb" "$before" > /dev/null 2>&1; check "passes" 0 $?

echo "a device with its own accessibility service (not a leak):"
state 'com.google.android.marvin.talkback/x|1|'
before=$(device_snapshot "$adb")
report_device_residue "$adb" "$before" > /dev/null 2>&1; check "unchanged state passes" 0 $?
assert_clean_baseline "$before" > /dev/null 2>&1; check "and the baseline is accepted" 0 $?

echo "a baseline already carrying glass's own residue:"
state 'com.fixedwidth.glassa11y/x|1|'
before=$(device_snapshot "$adb")
out=$(assert_clean_baseline "$before" 2>&1); check "is refused" 1 $?
grep -q 'already enabled' <<< "$out"; check "and says how to clear it" 0 $?
state 'null|0|emulator-5554 tcp:1 localabstract:glass-a11y'
out=$(assert_clean_baseline "$(device_snapshot "$adb")" 2>&1); check "a stale forward too" 1 $?

echo "more forwards after than before:"
state 'null|0|emulator-5554 tcp:1 localabstract:glass-a11y'
before=$(device_snapshot "$adb")
state 'null|0|emulator-5554 tcp:1 localabstract:glass-a11y;emulator-5554 tcp:2 localabstract:glass-a11y'
report_device_residue "$adb" "$before" > /dev/null 2>&1; check "counted, not deduplicated" 1 $?

echo "a bad settle value:"
state 'null|0|'
before=$(device_snapshot "$adb")
GLASS_RESIDUE_SETTLE=30s report_device_residue "$adb" "$before" > /dev/null 2>&1
check "is refused rather than skipping the check" 1 $?
GLASS_RESIDUE_SETTLE=08 report_device_residue "$adb" "$before" > /dev/null 2>&1
check "a zero-padded one is base 10, not octal" 0 $?

echo "passed=$pass failed=$fail"
[ "$fail" -eq 0 ]
