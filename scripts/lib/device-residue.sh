# shellcheck shell=bash
# Detect device state a suite switched on and never switched back off.
#
# `A11yServiceRegistry::ensure` enables the companion as an accessibility service and opens an
# `adb forward`; `AgentRegistry::ensure` opens one too. Only their `shutdown` puts that back, and a
# test panicking past a trailing `shutdown()` leaves the companion in the tree of every later
# reader on the device (glass#423), and the suite reports green while it happens.
#
# Compared against a snapshot taken BEFORE the run, never against a fixed "clean" value: a dev box
# may have its own accessibility service enabled, and asserting `null` would fail there forever.
# The one thing checked absolutely is glass's *own* residue — see `assert_clean_baseline`.

# Seconds to keep re-reading before calling a difference real, or an unreadable device dead.
: "${GLASS_RESIDUE_SETTLE:=15}"

# `SERVICE_PACKAGE` in crates/glass-android/src/a11y_service.rs, duplicated because a shell script
# cannot read a Rust `const`. Drift shows up as `assert_clean_baseline` never firing again.
GLASS_RESIDUE_SERVICE_PKG='com.fixedwidth.glassa11y'

# Run adb against the device the suite drives. `GLASS_ANDROID_SERIAL` is how a host with more than
# one device online picks it (crates/glass-android/src/adb.rs `build_argv` does the same), and
# without it every probe here fails on exactly the setup that variable exists for.
#
# `timeout` because the poll below only checks its deadline between reads, so a wedged device
# would hang a probe forever.
_residue_adb() {
    local adb="$1"
    shift
    if [ -n "${GLASS_ANDROID_SERIAL:-}" ]; then
        timeout 10 "$adb" -s "$GLASS_ANDROID_SERIAL" "$@"
    else
        timeout 10 "$adb" "$@"
    fi
}

# The `localabstract:glass-*` sockets forwarded for this device, sorted, space-separated.
_residue_forwards() {
    local adb="$1" list line
    local -a names=()
    list=$(_residue_adb "$adb" forward --list) || return 1
    while IFS= read -r line; do
        # `adb forward --list` is global — it ignores `-s` — so another device's forwards would
        # read as this one's.
        if [ -n "${GLASS_ANDROID_SERIAL:-}" ]; then
            case "$line" in "$GLASS_ANDROID_SERIAL "*) ;; *) continue ;; esac
        fi
        case "$line" in *" localabstract:glass-"*) names+=("${line##* }") ;; esac
    done <<< "$list"
    # Sorted for a stable comparison but NOT deduplicated: four leaked `glass-a11y` forwards are
    # four leaks, and `sort -u` would report them as the state one leaves.
    [ "${#names[@]}" -eq 0 ] || printf '%s' "$(printf '%s\n' "${names[@]}" | sort | tr '\n' ' ')"
}

# Print the device state this guard compares, one `key=value` line each, through the adb in $1.
# Non-zero if any probe failed — a device that cannot be read must not compare equal to another
# unreadable one, which is a guard that passes because it looked at nothing.
device_snapshot() {
    local adb="$1" services enabled forwards
    # The `shell` probes first: `adb forward --list` succeeds with no device attached, so leading
    # with it would let "nothing is connected" read as "nothing is forwarded".
    services=$(_residue_adb "$adb" shell settings get secure enabled_accessibility_services) || return 1
    enabled=$(_residue_adb "$adb" shell settings get secure accessibility_enabled) || return 1
    services=$(printf '%s' "$services" | tr -d '\r\n')
    enabled=$(printf '%s' "$enabled" | tr -d '\r\n')
    # A live device answers `settings get` with a value or the literal `null`, so nothing at all
    # means the probe did not run. `adb shell` does not carry the remote exit status on every
    # platform-tools version, which makes an empty reading the only sign of it.
    if [ -z "$services" ] || [ -z "$enabled" ]; then
        return 1
    fi
    # Normalised the way `A11yServiceRegistry::ensure` normalises before recording what to restore
    # (a11y_service.rs): an unset list becomes "" and an unset flag becomes "0", so a correct
    # teardown leaves `0` where it found `null` and a raw comparison would call that a leak.
    case "$services" in null) services="" ;; esac
    case "$enabled" in null | "") enabled=0 ;; esac
    forwards=$(_residue_forwards "$adb") || return 1

    printf 'enabled_accessibility_services=%s\n' "$services"
    printf 'accessibility_enabled=%s\n' "$enabled"
    printf 'forwards=%s\n' "$forwards"
}

# Refuse to run against a device that already carries glass's own residue.
#
# The before/after comparison cannot see it: `ensure` records the leaked service as the state to
# restore, puts that same state back, and every run after the first reports clean on a device that
# never got cleaned. Checked absolutely because these names are glass's own — unlike a dev box's
# TalkBack, they are never legitimately on before a run.
assert_clean_baseline() {
    local snapshot="$1" bad=""
    case "$snapshot" in
        *"$GLASS_RESIDUE_SERVICE_PKG"*) bad="the glass companion is already enabled" ;;
    esac
    case "$snapshot" in
        *"forwards=localabstract:glass-"*) bad="${bad:+$bad, and }a glass forward is already open" ;;
    esac
    [ -n "$bad" ] || return 0
    echo "FAIL: $bad before this run started — an earlier run leaked it." >&2
    printf '%s\n' "$snapshot" >&2
    echo "Clear it, or this check can never see a new leak:" >&2
    echo "  adb shell settings delete secure enabled_accessibility_services" >&2
    echo "  adb shell settings put secure accessibility_enabled 0" >&2
    echo "  adb forward --remove-all" >&2
    return 1
}

# Compare the device against the snapshot in $2, retrying until it settles. Non-zero, and loud, if
# it has not gone back to $2 within `GLASS_RESIDUE_SETTLE` seconds.
report_device_residue() {
    local adb="$1" before="$2" after last="" looks=0 started=$SECONDS deadline
    # Validated, because an arithmetic error in the deadline below would skip the comparison and
    # the run would pass having checked nothing. `10#` so `08` is eight seconds, not a base-8 error.
    case "$GLASS_RESIDUE_SETTLE" in
        '' | *[!0-9]*)
            echo "FAIL: GLASS_RESIDUE_SETTLE must be whole seconds, got '$GLASS_RESIDUE_SETTLE'" >&2
            return 1
            ;;
    esac
    deadline=$((SECONDS + 10#$GLASS_RESIDUE_SETTLE))

    while :; do
        looks=$((looks + 1))
        if after=$(device_snapshot "$adb"); then
            if [ "$after" = "$before" ]; then
                # Said out loud so a run that skipped the check cannot be mistaken for a clean one.
                echo "device-residue: clean after $looks look(s), $((SECONDS - started))s" >&2
                return 0
            fi
            last=differs
        else
            # Retried, not fatal on the first miss: the suite has just finished hammering the
            # device, and in CI the emulator is about to be torn down around it.
            last=unreadable
        fi
        [ "$SECONDS" -lt "$deadline" ] || break
        sleep 1
    done

    if [ "$last" = unreadable ]; then
        echo "FAIL: could not read the device for ${GLASS_RESIDUE_SETTLE}s after the suite" >&2
        return 1
    fi
    # A fixed marker, because the exit status is lost when the suite failed too — which is the
    # case this guard exists for, a test panicking past its own teardown.
    echo "GLASS-RESIDUE: LEAKED" >&2
    echo "The suite left device state behind (- before the run, + after):" >&2
    diff <(printf '%s\n' "$before") <(printf '%s\n' "$after") >&2 || true
    echo "A test never reached its registry \`shutdown\`; an enabled companion is in the tree of" >&2
    echo "every later reader on this device." >&2
    return 1
}
