# shellcheck shell=bash
# Detect sessions that never tore down.
#
# A session's private dirs — the compositor's runtime dir and the a11y bus dir — are removed by
# the same teardown that reaps their processes, so one surviving a run is a live sway or
# dbus-daemon nobody owns (glass#415, glass#418). The tests pass while it happens, which is why
# this is checked around a suite rather than asserted inside one.

# A file whose mtime is "now". Pass it to `session_residue`, then delete it.
residue_marker() { mktemp; }

# Print the session dirs created since $1, one per line. Empty means none. Scoping to "newer
# than the marker" keeps an earlier run's residue from failing this one.
session_residue() {
    find /tmp -maxdepth 1 \
        \( -name 'glass-wl.*' -o -name 'glass-a11y-*' \) \
        -newer "$1" 2>/dev/null || true
}

# Run `session_residue`, report to stderr, and echo the exit status the caller should use:
# $1 marker, $2 the status the suite itself produced.
report_residue() {
    local marker="$1" status="$2" leaked
    leaked=$(session_residue "$marker")
    rm -f "$marker"
    if [ -n "$leaked" ]; then
        echo "FAIL: the suite leaked $(printf '%s\n' "$leaked" | wc -l) session(s):" >&2
        printf '%s\n' "$leaked" >&2
        echo "Each is a live compositor or a11y bus; a test never stopped its session." >&2
        status=1
    fi
    echo "$status"
}
