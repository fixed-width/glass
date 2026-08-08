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
#   scripts/mutants.sh target/mutants --package glass-android --test-tool nextest
#   scripts/mutants.sh target/mutants --package glass-core --package glass-x11
#
# `--package` is read here, not forwarded: it sets the `--file` glob too, and forwarding it
# alone would mutate only the default package's files under a widened package set — a clean
# run over a crate that was never in scope. Repeat it for more.
#
# Ignored tests are run, and that is set here rather than left to the caller: a crate whose
# display-backed tests are `#[ignore]`d would otherwise have every mutant those tests cover come
# back a survivor, failing the run over code that is in fact tested.
#
# The hazard in the other direction is narrower than it looks, and worth stating exactly, because
# the obvious guess is wrong. cargo-mutants grades a mutant caught when the test command exits
# NON-ZERO (`outcome.rs`), so a run that tests nothing and exits 0 is a survivor — loud. Only one
# shape is silent: nextest exits 4 when no test matched, and cargo-mutants lists that among its
# allowed nextest codes, so a package left with zero tests grades every mutant caught with no
# warning. Keep every gated package's test set non-empty (see the filterset below).
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

# Running the ignored tests reaches glass-android's device tests, which have no device here.
# Excluded rather than made to self-skip: a hardware test that passes without its hardware
# asserts nothing and says so nowhere (the same reasoning as glass-macos `process.rs`).
#
# This filterset must leave at least one test in every gated package — empty is the one input
# nextest reports in a way cargo-mutants reads as every mutant caught (see the header).
readonly NEEDS_A_DEVICE='(package(glass-android) and kind(test))
    or test(=adb::tests::a_shell_call_that_never_answers_dies_at_its_budget)'

# Run the `#[ignore]`d tests too (see the header). cargo-mutants appends these to the end of the
# test command and to the test phase only, so the nextest flag needs no `--` and the cargo one
# does.
case " ${passthrough[*]-} " in
    *" --test-tool nextest "* | *" --test-tool=nextest "*)
        run_ignored=(--cargo-test-arg=--run-ignored=all
            --cargo-test-arg=-E --cargo-test-arg="not ($NEEDS_A_DEVICE)")
        ;;
    *)
        # `cargo test` takes no filterset, so the device tests cannot be excluded here.
        if printf '%s\n' "${packages[@]}" | grep -qx glass-android; then
            echo "glass-android's tests need a device, and only the nextest path can filter" >&2
            echo "them out — re-run with --test-tool nextest." >&2
            exit 2
        fi
        run_ignored=(--cargo-test-arg=-- --cargo-test-arg=--include-ignored)
        ;;
esac

# How many mutants a set of scope arguments yields, ignoring any `--shard` the caller
# passed. Listing costs ~0.1s and builds nothing, so the "did this gate anything"
# question is answered before a single mutant is compiled — and answered for the whole
# run rather than for one shard, which may legitimately receive none.
list_count() {
    # stderr to a file, not into stdout: it is the count of the lines below, and a cargo
    # warning merged in would inflate it. Discarding it instead — what this used to do — makes
    # a failed `--list` take the whole script down under `set -e` with an empty log, never
    # reaching the "gated nothing" message.
    local out err rc=0
    err=$(mktemp)
    out=$(cargo mutants --list "${pkg_args[@]}" "$@" \
        ${diff_file:+--in-diff "$diff_file"} 2>"$err") || rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "cargo mutants --list failed, so whether this run would gate anything is unknown:" >&2
        cat "$err" >&2
        rm -f "$err"
        exit 2
    fi
    rm -f "$err"
    # `printf` on an empty string still emits a newline, which `wc -l` would count as one mutant.
    [ -z "$out" ] && { echo 0; return; }
    printf '%s\n' "$out" | wc -l
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
        "${run_ignored[@]}" \
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

# A shard may legitimately draw none of the planned mutants; the count above is what
# proves the run as a whole gated something.
echo "mutants: $total generated, $caught caught, $missed missed, $timed_out timed out, $unviable unviable"
if [ "$missed" -gt 0 ]; then
    echo "Survivors are listed in $graded/mutants.out/missed.txt"
    exit 1
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
