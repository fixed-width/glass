#!/usr/bin/env bash
# Mutation-test one or more crates with cargo-mutants and grade the outcome.
#
# Why this wrapper rather than a bare `cargo mutants`: its exit code alone cannot
# tell a clean run from one that gated nothing.
#
#   * It exits 0 whenever it generates no mutants at all — a `--file` glob that
#     stopped matching after a file moved, or a package that was renamed. Both
#     look exactly like success.
#   * It reports a timeout ahead of a missed mutant, so once any mutant times out a
#     genuine survivor is invisible at the exit code.
#
# So this reads `outcomes.json`, prints the breakdown, and fails on a survivor or a
# run that gated nothing.
#
# A timeout is not a failure here: every one this crate produces is a mutation that stops
# the code terminating, and no test can catch a hang — the timeout budget is what catches
# it. The count and `timeout.txt` are still printed, because a mutant that merely got slow
# looks the same.
#
# The one legitimate way to generate no mutants is a diff that changes only test
# code. That must not fail — but it must not pass unchecked either, because
# deleting the test that kills a survivor brings the survivor back while the diff
# itself contains nothing to mutate. So a `--in-diff` run that yields nothing falls
# back to mutating the whole of each file the diff touched.
#
# Usage:
#   scripts/mutants.sh <output-dir> [--package NAME]... [extra cargo-mutants args...]
#
#   scripts/mutants.sh target/mutants                      # the whole default crate
#   scripts/mutants.sh target/mutants --in-diff pr.diff    # only what a diff touched
#   scripts/mutants.sh target/mutants --package glass-android
#   scripts/mutants.sh target/mutants --package glass-core --package glass-android
#
# `--package` is read here, not forwarded: it sets the `--file` glob too, and forwarding it
# alone would mutate only the default package's files under a widened package set — a clean
# run over a crate that was never in scope. Repeat it for more.
#
# MUTANTS_JOBS sets concurrency (default 2); `--jobs` cannot be passed through,
# because cargo-mutants rejects it twice over.
set -euo pipefail

if [ "$#" -lt 1 ]; then
    echo "usage: scripts/mutants.sh <output-dir> [cargo-mutants args...]" >&2
    exit 2
fi
out=$1
shift

# glass-core is the platform-agnostic heart — the Platform/Accessibility seams, session,
# frame diffing, stability, the log buffer — so it is pure logic that mutates meaningfully and
# tests without a display. CI names its packages explicitly, so this default now covers only a
# bare local run — `--package` scopes a run to other crates.
readonly DEFAULT_PACKAGE='glass-core'

# A package's own sources. `**` so a file moved into a subdirectory stays covered; keep this in
# step with the git pathspec the in-diff caller uses.
package_glob() { echo "crates/$1/src/**/*.rs"; }

# Fixed rather than derived from the unmutated baseline: cargo-mutants ranks a timeout above a
# missed mutant, so once anything times out a genuine survivor is invisible at the exit code. A
# generous explicit value keeps the grade from depending on how loaded the host is.
readonly MUTANT_TIMEOUT=${MUTANT_TIMEOUT:-180}

# The caller's `--in-diff` path and `--package` names, and the same argument list with both
# removed — the fallback re-runs without `--in-diff` but must keep everything else,
# `--test-tool` included.
diff_file=""
packages=()
passthrough=()
want=""
for arg in "$@"; do
    if [ -n "$want" ]; then
        case "$want" in
            in-diff) diff_file=$arg ;;
            package) packages+=("$arg") ;;
        esac
        want=""
        continue
    fi
    case "$arg" in
        --in-diff) want=in-diff ;;
        --in-diff=*) diff_file=${arg#--in-diff=} ;;
        --package | -p) want=package ;;
        --package=*) packages+=("${arg#--package=}") ;;
        *) passthrough+=("$arg") ;;
    esac
done
# A flag left waiting for its value would otherwise swallow the next one silently.
if [ -n "$want" ]; then
    echo "--$want was given no value" >&2
    exit 2
fi
[ "${#packages[@]}" -eq 0 ] && packages=("$DEFAULT_PACKAGE")

# `--package p1 --package p2 …`, the form both cargo-mutants calls below want.
pkg_args=()
for p in "${packages[@]}"; do
    pkg_args+=(--package "$p")
done

# How many mutants a set of scope arguments yields, ignoring any `--shard` the caller
# passed. Listing costs ~0.1s and builds nothing, so the "did this gate anything"
# question is answered before a single mutant is compiled — and answered for the whole
# run rather than for one shard, which may legitimately receive none.
list_count() {
    cargo mutants --list "${pkg_args[@]}" "$@" \
        ${diff_file:+--in-diff "$diff_file"} 2>/dev/null | wc -l
}

total=0 caught=0 missed=0 timed_out=0 unviable=0 status=0

# Run cargo-mutants into $1 with the remaining arguments, and read the outcome counts.
# A missing outcomes.json means it generated nothing and wrote no report at all.
attempt() {
    local dir=$1
    shift
    status=0
    cargo mutants \
        "${pkg_args[@]}" \
        --cargo-arg=--locked \
        --timeout "$MUTANT_TIMEOUT" \
        -j "${MUTANTS_JOBS:-2}" \
        --output "$dir" \
        "$@" || status=$?

    local outcomes="$dir/mutants.out/outcomes.json"
    if [ -f "$outcomes" ]; then
        read -r total caught missed timed_out unviable < <(
            jq -r '[.total_mutants, .caught, .missed, .timeout, .unviable] | @tsv' "$outcomes"
        )
    else
        total=0 caught=0 missed=0 timed_out=0 unviable=0
    fi
}

# Choose the scope before building anything: the module glob, or — when the diff
# changed only test code and so yields nothing to mutate — the whole of each file it
# touched, so a deleted test cannot bring a survivor back unnoticed.
scope=()
for p in "${packages[@]}"; do
    scope+=(--file "$(package_glob "$p")")
done
planned=$(list_count "${scope[@]}")

if [ "$planned" -eq 0 ] && [ -n "$diff_file" ] && [ -s "$diff_file" ]; then
    # `+++ b/<path>` names each file the diff writes to; a deletion names /dev/null
    # and drops out with the `.rs` filter.
    mapfile -t touched < <(
        sed -n 's|^+++ b/||p' "$diff_file" | grep -E '\.rs$' | sort -u
    )
    if [ "${#touched[@]}" -gt 0 ]; then
        echo "The diff changed no mutable code — only tests, or only comments."
        echo "Falling back to the whole of each file it touched, so a deleted test"
        echo "cannot bring a survivor back unnoticed:"
        printf '  %s\n' "${touched[@]}"
        scope=()
        for f in "${touched[@]}"; do
            scope+=(--file "$f")
        done
        diff_file=""
        planned=$(list_count "${scope[@]}")
    fi
fi

if [ "$planned" -eq 0 ]; then
    echo "This run would gate nothing: no mutants under the scope it was given."
    echo "Scoped to: ${packages[*]}"
    echo "Check that --file still matches the files under test and that each package"
    echo "still exists."
    exit 1
fi
echo "mutants planned across the whole run: $planned"

attempt "$out" "${scope[@]}" ${diff_file:+--in-diff "$diff_file"} \
    ${passthrough[@]+"${passthrough[@]}"}
graded=$out

# Reap the compositors and X servers a timed-out mutant left behind.
#
# The mutation runner SIGKILLs the test process when a mutant exceeds --timeout, so a mutation
# that removes a *bound* — a discovery deadline, a screencopy event arm, a Drop impl — hangs the
# run while holding live sessions and none of their teardown ever runs. Each orphan holds an X
# display number until it dies, and a host with none left cannot start Xwayland at all.
#
# This is cleanup after the fact. What stops the orphans accumulating mid-run is the pdeathsig in
# glass-wayland's `build_sway_command`; this catches what escapes it.
#
# Matched on the private runtime directory glass names for each session, which appears on the
# compositor's own command line and belongs to no one else. Deliberately NOT matched on `Xvfb
# -displayfd`: glass-x11 spawns its servers with nothing glass-specific in the argv, and its own
# docs tell a developer to run that exact command, so an orphaned one may well be theirs.
#
# Only orphans, so a session whose parent is alive belongs to a running test. TERM only: a KILL
# leaves behind the sockets and children that this process's own teardown is what removes.
reap_strays() {
    local pids
    if ! pids=$(pgrep -P 1 -f 'glass-wl\.|glass-doctor-wl\.'); then
        return 0 # pgrep exits 1 for no match
    fi
    echo "mutants: reaping $(echo "$pids" | wc -w) session(s) a timed-out mutant left running"
    # shellcheck disable=SC2086 # deliberate word splitting: one signal per pid
    kill -TERM $pids 2>/dev/null || true
}
reap_strays

# A shard may legitimately draw none of the planned mutants; the count above is what
# proves the run as a whole gated something.
echo "mutants: $total generated, $caught caught, $missed missed, $timed_out timed out, $unviable unviable"

# Grade survivors against a recorded set rather than against zero.
#
# Without this, a survivor on a line nobody has touched is invisible — until someone edits that
# line for an unrelated reason and the in-diff gate hands them a mutant they did not write. Their
# choices are then to do deep unrelated work, add an exclusion under deadline pressure, or turn
# the gate off. That is how gates get turned off.
#
# So the record applies to BOTH gates: a recorded survivor never fails anyone, a new one always
# does, and one that has become killable says so. This is a ratchet, not an amnesty — the bar is
# "no new survivors", and adding to the record is a reviewed diff that has to carry a reason.
#
# Deliberately not `exclude_re` in mutants.toml: an exclusion stops the mutant being generated, so
# it can never be re-graded and never reports that it has become killable. These are still
# generated, still run, and still printed every time.
#
# Per crate, so a crate at zero stays at zero: a record exists only for the crates that have one,
# and a crate with no file has no slot to record into.
record_for() { echo ".cargo/mutants-known/$1.txt"; }

# A survivor keyed by its description and how many share it, with `line:col` dropped — the line
# moves whenever anything above it does. Two mutants can share a description at different lines,
# so the count is what stops one of a pair being fixed unnoticed. Renaming the function a survivor
# names DOES change its key, deliberately: its recorded reason may no longer fit the code.
survivor_keys() {
    # The trailing sort is not redundant: `uniq -c` prefixes a count, which reorders the lines,
    # and `comm` silently mis-compares unsorted input.
    sed -E 's/^([^:]+):[0-9]+:[0-9]+: /\1: /' "$1" | sort | uniq -c | sed -E 's/^ +//' | sort
}

recorded_keys() {
    local f
    for p in "${packages[@]}"; do
        f=$(record_for "$p")
        [ -f "$f" ] && grep -vE '^\s*(#|$)' "$f" | sed -E 's/^ +//'
    done | sort
}

grade_survivors() {
    local missed_txt="$graded/mutants.out/missed.txt"
    local now expected new_ones fixed_ones
    now=$(mktemp) && expected=$(mktemp)
    # Compared even when nothing survived: that is exactly when a recorded entry has become
    # killable, and the run should say so rather than quietly passing.
    if [ -f "$missed_txt" ]; then survivor_keys "$missed_txt" > "$now"; else : > "$now"; fi
    recorded_keys > "$expected"

    new_ones=$(comm -23 "$now" "$expected")
    fixed_ones=$(comm -13 "$now" "$expected")

    # Printed every run, so a recorded survivor stays visible instead of becoming furniture.
    if [ -s "$expected" ]; then
        echo "Recorded survivors (see .cargo/mutants-known/):"
        sed 's/^/  /' "$expected"
    fi
    if [ -n "$fixed_ones" ]; then
        echo
        echo "These are now caught — delete them from .cargo/mutants-known/:"
        echo "$fixed_ones" | sed 's/^/  /'
    fi
    if [ -n "$new_ones" ]; then
        echo
        echo "Survivors that are not recorded:"
        echo "$new_ones" | sed 's/^/  /'
        echo
        echo "Kill them, or record each with the reason it cannot be killed. A reason is a"
        echo "property of the code — equivalent, unreachable, only reachable by a race — not"
        echo "'hard' and not 'not mine'. Full list: $missed_txt"
        rm -f "$now" "$expected"
        return 1
    fi
    rm -f "$now" "$expected"
    return 0
}

if ! grade_survivors; then
    exit 1
fi
# Every survivor was recorded, so cargo-mutants' own "found problems" status must not fail the run
# — otherwise the record grades nothing and the exit code decides after all. Exit 2 is exactly
# that outcome (`ExitCode::FoundProblems`); a usage error, a failing baseline or a bad filter diff
# all have their own codes and still stand.
if [ "$missed" -gt 0 ] && [ "$status" -eq 2 ]; then
    status=0
fi
if [ "$timed_out" -gt 0 ]; then
    echo "Timed-out mutants are listed in $graded/mutants.out/timeout.txt — a hang is a"
    echo "detection, not a survivor, so they do not fail this run. Read them anyway: a"
    echo "mutant that merely got slow, or a budget set too low, looks exactly the same."
    # Exit 3 is cargo-mutants' "some tests timed out", the outcome just graded. An `if`
    # rather than an `&&` list: a false `&&` is itself a failed command under `set -e`.
    if [ "$status" -eq 3 ]; then
        status=0
    fi
fi
exit "$status"
