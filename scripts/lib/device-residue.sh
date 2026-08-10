# shellcheck shell=bash
# Detect device state a suite switched on and never switched back off.
#
# `A11yServiceRegistry::ensure` enables the companion as an accessibility service and opens an
# `adb forward`; `AgentRegistry::ensure` opens one too. Only their `shutdown` puts that back, and a
# test panicking past a trailing `shutdown()` leaves the companion in the tree of every later
# reader on the device (glass#423). The suite reports green while it happens, hence a check around
# it rather than an assertion inside one test.
#
# Compared against a snapshot taken BEFORE the run, never against a fixed "clean" value: a dev box
# may have its own accessibility service enabled, and asserting `null` would fail there forever.

# How long to keep re-reading before calling a difference real. The service unbind is async by
# about a second, so a single look right after the suite reads a mid-teardown device as a leak.
: "${GLASS_RESIDUE_SETTLE:=15}"

# Print the device state this guard compares, one `key=value` line each, through the adb in $1.
# Non-zero if any probe failed — a device that cannot be read must not compare equal to another
# unreadable one, which is a guard that passes because it looked at nothing.
device_snapshot() {
    local adb="$1" services enabled forwards
    services=$("$adb" shell settings get secure enabled_accessibility_services) || return 1
    enabled=$("$adb" shell settings get secure accessibility_enabled) || return 1
    forwards=$("$adb" forward --list) || return 1
    # Only the socket names: the local tcp port is assigned per run, so the raw lines differ
    # between two equally clean snapshots.
    forwards=$(printf '%s\n' "$forwards" | grep -o 'localabstract:glass-[a-z0-9-]*' | sort -u) ||
        [ $? -eq 1 ] || return 1
    printf 'enabled_accessibility_services=%s\n' "$(printf '%s' "$services" | tr -d '\r')"
    printf 'accessibility_enabled=%s\n' "$(printf '%s' "$enabled" | tr -d '\r')"
    printf 'forwards=%s\n' "$(printf '%s' "$forwards" | tr '\n' ' ')"
}

# Compare the device against the snapshot in $2, polling until it settles. Non-zero, and loud, if
# it has not gone back to $2 within `GLASS_RESIDUE_SETTLE` seconds.
report_device_residue() {
    local adb="$1" before="$2" after deadline=$((SECONDS + GLASS_RESIDUE_SETTLE))
    while :; do
        if ! after=$(device_snapshot "$adb"); then
            echo "FAIL: could not read the device state after the suite" >&2
            return 1
        fi
        [ "$after" = "$before" ] && return 0
        [ "$SECONDS" -ge "$deadline" ] && break
        sleep 1
    done
    echo "FAIL: the suite left device state behind (- before the run, + after):" >&2
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") >&2 || true
    echo "A test never reached its registry \`shutdown\`; an enabled companion is in the tree of" >&2
    echo "every later reader on this device." >&2
    return 1
}
