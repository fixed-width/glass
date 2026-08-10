# shellcheck shell=bash
# Detect sessions that never tore down.
#
# A session's private dirs — the compositor's runtime dir and the a11y bus dir — are removed by
# the same teardown that reaps their processes, so one surviving a run is a live sway or
# dbus-daemon nobody owns (glass#415, glass#418). The tests pass while it happens, hence a check
# around the suite rather than an assertion inside one.
#
# Both dirs follow `$TMPDIR` as `mktemp` does — scan where they are created, or this silently
# passes forever.

# A marker file whose mtime is "now"; the caller removes it. Do NOT trap EXIT in here: callers
# read this through `$(…)`, whose subshell fires the trap and deletes the marker before it is
# ever compared against, leaving a scan that finds nothing and passes.
residue_marker() {
    mktemp "${TMPDIR:-/tmp}/glass-residue-marker.XXXXXX"
}

# Report session dirs modified since marker $1; non-zero if there were any. The remaining args
# are the globs this suite can itself produce — matching one it cannot lets a concurrent suite's
# live scratch read as this one's leak.
#
# Six `?` matches `tempfile`'s random suffix and nothing else: `test-a11y.sh`'s own
# `glass-a11y-test-rt.XXXXXX` runtime dir is scratch, not residue.
report_residue() {
    local marker="$1" leaked="" glob hits
    shift
    for glob in "$@"; do
        # A failed scan must not read as a clean one — that is how a leak detector goes quiet.
        if ! hits=$(find "${TMPDIR:-/tmp}" -maxdepth 1 -name "$glob" -newer "$marker"); then
            echo "FAIL: could not scan for session residue (marker $marker)" >&2
            return 1
        fi
        if [ -n "$hits" ]; then
            leaked="${leaked}${hits}"$'\n'
        fi
    done
    leaked="${leaked%$'\n'}"
    if [ -z "$leaked" ]; then
        return 0
    fi
    echo "FAIL: the suite leaked $(printf '%s\n' "$leaked" | wc -l) session(s):" >&2
    printf '%s\n' "$leaked" >&2
    echo "Each is a live compositor or a11y bus; a test never stopped its session." >&2
    return 1
}
