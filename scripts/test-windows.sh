#!/usr/bin/env bash
# Run glass-windows on-box validation on a REMOTE Windows box from Linux. Unlike the local
# test-x11.sh / test-wayland.sh suites, the Windows box is remote and WGC/SendInput need its
# interactive console session, so this orchestrates over SSH: push the base commit, ship any
# uncommitted working-tree delta, then invoke tools/windows-validation/run-onbox.ps1 on the box
# (which does the session-1 bounce). Skips cleanly when no box is configured.
#
# Config (env; nothing box-specific is committed to this public repo):
#   GLASS_WIN_HOST       user@host for SSH (required; unset => skip)
#   GLASS_WIN_REPO       remote checkout path (default C:/Users/<user>/glass)
#   GLASS_WIN_SSH_OPTS   extra ssh/scp options (optional)
#
# Usage:
#   ./scripts/test-windows.sh                 # all onbox_* examples
#   ./scripts/test-windows.sh onbox_handoff   # one (or several) named examples
#   ./scripts/test-windows.sh --all           # explicit all
#   ./scripts/test-windows.sh --tests clip    # cargo test -- --ignored clip
#   ./scripts/test-windows.sh --release ...    # release profile
#   ./scripts/test-windows.sh --dry-run ...    # print the plan, no SSH
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${GLASS_WIN_HOST:-}"
REPO="${GLASS_WIN_REPO:-}"
SSH_OPTS="${GLASS_WIN_SSH_OPTS:-}"

DRY_RUN=0; RELEASE=0; TESTS=""
declare -a TARGETS=()
ALL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --all) ALL=1 ;;
    --tests) TESTS="${2:-}"; shift ;;
    --release) RELEASE=1 ;;
    --dry-run) DRY_RUN=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    --*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) TARGETS+=("$1") ;;
  esac
  shift
done

if [ -z "$HOST" ]; then
  echo "windows box not configured (set GLASS_WIN_HOST=user@host [GLASS_WIN_REPO=C:/path]); skipping."
  exit 0
fi
RUSER="${HOST%@*}"
[ -n "$REPO" ] || REPO="C:/Users/${RUSER}/glass"
case "$HOST$REPO" in
  *[[:space:]]*) echo "error: GLASS_WIN_HOST/GLASS_WIN_REPO must not contain spaces" >&2; exit 2 ;;
esac

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$BRANCH" = "HEAD" ]; then
  echo "error: detached HEAD; check out a branch before running test-windows.sh" >&2
  exit 1
fi
SHA="$(git rev-parse HEAD)"
DIRTY=0
if ! git diff --quiet HEAD || [ -n "$(git ls-files --others --exclude-standard)" ]; then DIRTY=1; fi

# Assemble the bridge invocation args.
PS_ARGS=( -RepoDir "$REPO" -Branch "$BRANCH" -Sha "$SHA" )
[ "$ALL" -eq 1 ] && PS_ARGS+=( -All )
[ ${#TARGETS[@]} -gt 0 ] && PS_ARGS+=( -Targets "$(IFS=,; echo "${TARGETS[*]}")" )
[ -n "$TESTS" ] && PS_ARGS+=( -Tests "$TESTS" )
[ "$RELEASE" -eq 1 ] && PS_ARGS+=( -Release )

if [ "$DRY_RUN" -eq 1 ]; then
  echo "[dry-run] host=$HOST repo=$REPO branch=$BRANCH sha=${SHA:0:8} dirty=$DIRTY"
  if [ "$DIRTY" -eq 1 ]; then
    echo "[dry-run] sync: git push origin HEAD:$BRANCH; scp diff+untracked; box resets to $SHA then applies delta"
  else
    echo "[dry-run] sync: git push origin HEAD:$BRANCH; box checks out $BRANCH and resets to $SHA"
  fi
  echo "[dry-run] invoke: run-onbox.ps1 ${PS_ARGS[*]}"
  echo "[dry-run] then: scp $REPO/.windows-artifacts -> ./.windows-artifacts/"
  exit 0
fi

# shellcheck disable=SC2086
sshx() { ssh $SSH_OPTS -o ConnectTimeout=8 "$HOST" "$@"; }

# --- preflight ---
if ! sshx "cmd /c echo ok" >/dev/null 2>&1; then
  echo "error: $HOST unreachable -- powered up with sshd running?" >&2
  exit 1
fi

# --- sync: push base commit, ship working-tree delta ---
echo "== push base commit ($BRANCH @ ${SHA:0:8}) =="
git push -q origin "HEAD:$BRANCH"

DIFF_REMOTE=""; UNTAR_REMOTE=""
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
if [ "$DIRTY" -eq 1 ]; then
  echo "== ship working-tree delta =="
  git diff HEAD --binary > "$TMP/wip.diff"
  git ls-files --others --exclude-standard -z | tar --null -cf "$TMP/untracked.tar" --files-from=- 2>/dev/null || : > "$TMP/untracked.tar"
  # shellcheck disable=SC2086
  scp $SSH_OPTS -q "$TMP/wip.diff" "$HOST:$REPO/.glass-wip.diff"
  # shellcheck disable=SC2086
  scp $SSH_OPTS -q "$TMP/untracked.tar" "$HOST:$REPO/.glass-untracked.tar"
  DIFF_REMOTE="$REPO/.glass-wip.diff"; UNTAR_REMOTE="$REPO/.glass-untracked.tar"
  PS_ARGS+=( -DiffPath "$DIFF_REMOTE" -UntarPath "$UNTAR_REMOTE" )
fi

# --- invoke the bridge ---
echo "== run on box =="
set +e
# shellcheck disable=SC2086
sshx "powershell -NoProfile -ExecutionPolicy Bypass -File \"$REPO/tools/windows-validation/run-onbox.ps1\" ${PS_ARGS[*]}"
RC=$?
set -e

# --- pull artifacts (best-effort) ---
mkdir -p ./.windows-artifacts
# shellcheck disable=SC2086
scp $SSH_OPTS -q -r "$HOST:$REPO/.windows-artifacts/." ./.windows-artifacts/ 2>/dev/null || true
echo "artifacts -> ./.windows-artifacts/ (rc=$RC)"
exit $RC
