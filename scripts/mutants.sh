#!/usr/bin/env bash
# Mutation-test the smoke module with cargo-mutants and grade the outcome.
#
# Why this wrapper rather than a bare `cargo mutants`: its exit code alone cannot
# tell a clean run from one that gated nothing.
#
#   * It exits 0 whenever it generates no mutants at all — a diff that touches the
#     module only inside `#[cfg(test)]`, a `--file` glob that stopped matching after
#     a file moved, a package that was renamed. All three look like success.
#   * It reports a timeout ahead of a missed mutant, so once any mutant times out a
#     genuine survivor is invisible at the exit code.
#
# So this reads `outcomes.json`, prints the breakdown, and fails on a survivor, a
# timeout, or a run that generated nothing.
#
# Usage:
#   scripts/mutants.sh <output-dir> [extra cargo-mutants args...]
#
#   scripts/mutants.sh target/mutants                     # the whole module
#   scripts/mutants.sh target/mutants --in-diff pr.diff   # only what a diff touched
set -euo pipefail

if [ "$#" -lt 1 ]; then
    echo "usage: scripts/mutants.sh <output-dir> [cargo-mutants args...]" >&2
    exit 2
fi
out=$1
shift

# `**` so a file moved into a subdirectory of the module stays covered; keep this in
# step with the git pathspec the in-diff caller uses.
readonly SMOKE_GLOB='crates/glass-mcp/src/smoke/**/*.rs'

status=0
cargo mutants \
    --package glass-mcp \
    --file "$SMOKE_GLOB" \
    --cargo-arg=--locked \
    -j "${MUTANTS_JOBS:-2}" \
    --output "$out" \
    "$@" || status=$?

outcomes="$out/mutants.out/outcomes.json"
if [ ! -f "$outcomes" ]; then
    echo "mutants tested: 0 — cargo-mutants generated nothing and wrote no outcome."
    echo "This run gated nothing. Check that --file still matches the files under test"
    echo "and that the package still exists."
    exit 1
fi

read -r total caught missed timed_out unviable < <(
    jq -r '[.total_mutants, .caught, .missed, .timeout, .unviable] | @tsv' "$outcomes"
)
echo "mutants: $total generated, $caught caught, $missed missed, $timed_out timed out, $unviable unviable"

if [ "$total" -eq 0 ]; then
    echo "This run gated nothing. Check that --file still matches the files under test"
    echo "and that the package still exists."
    exit 1
fi
if [ "$missed" -gt 0 ]; then
    echo "Survivors are listed in $out/mutants.out/missed.txt"
    exit 1
fi
if [ "$timed_out" -gt 0 ]; then
    echo "A timeout also masks any survivor at cargo-mutants' own exit code."
    echo "Timed-out mutants are listed in $out/mutants.out/timeout.txt"
    exit 1
fi
exit "$status"
