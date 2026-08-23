#!/usr/bin/env bash
# scripts/sync-upstream-selfdev.sh — one-shot fork maintenance:
#
#   1. fetch $UPSTREAM_REMOTE and fast-forward local master
#   2. merge master into the current branch (skip when already on master)
#   3. run the jcode-base regression gates (skip: SKIP_TESTS=1)
#   4. publish a fresh self-dev build (jcode self-dev --build)
#   5. reload the shared daemon onto it
#
# Env overrides:
#   UPSTREAM_REMOTE (default "upstream")   NO_PUSH=1        don't push master
#   SKIP_TESTS=1                           CANARY=1         keep the canary
#                                                           TUI window alive
#   BUILD_TIMEOUT_S (default 1800)
#
# Run from anywhere; the repo root is derived from this script's location.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UPSTREAM="${UPSTREAM_REMOTE:-upstream}"
BUILD_TIMEOUT_S="${BUILD_TIMEOUT_S:-1800}"
cd "$REPO"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing dependency: $1" >&2; exit 3; }; }
need git
need cargo
need jq

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "== repo: $REPO (branch: $BRANCH)"

git remote get-url "$UPSTREAM" >/dev/null 2>&1 \
  || { echo "no git remote '$UPSTREAM' (git remote add $UPSTREAM <upstream-url>)" >&2; exit 3; }

echo "== fetching $UPSTREAM"
git fetch "$UPSTREAM" --prune
UP_MASTER="$(git rev-parse --verify --quiet "$UPSTREAM/master")" \
  || { echo "$UPSTREAM/master not found" >&2; exit 3; }

sync_master() {
  if git merge-base --is-ancestor "$UP_MASTER" master; then
    echo "== master already contains $UPSTREAM/master"
    return
  fi
  git checkout master
  if git merge-base --is-ancestor master "$UP_MASTER"; then
    local behind
    behind="$(git rev-list --count master.."$UP_MASTER")"
    echo "== fast-forwarding master ($behind commits from $UPSTREAM)"
    git merge --ff-only "$UP_MASTER"
    if [ "${NO_PUSH:-0}" != "1" ]; then
      git push origin master
    fi
    return
  fi
  # Diverged: fork master only ever carries squashes of our own branches, so
  # rebase onto upstream to stay linear. Force-push is lease-protected.
  local ahead
  ahead="$(git rev-list --count "$UP_MASTER"..master)"
  echo "== master diverged from $UPSTREAM (ahead: $ahead); rebasing"
  if ! git rebase "$UP_MASTER" master; then
    echo "" >&2
    echo "REBASE CONFLICT on master: resolve, 'git rebase --continue', then re-run this script." >&2
    exit 4
  fi
  if [ "${NO_PUSH:-0}" != "1" ]; then
    git push origin master --force-with-lease
  fi
}

if [ "$BRANCH" = "master" ]; then
  sync_master
else
  # Sync master first so the feature branch merges the true integration line,
  # then bring the branch up to date. Conflicts are left in place for the user;
  # the script refuses to build a half-merged tree.
  sync_master
  git checkout "$BRANCH"
  local_behind_master="$(git rev-list --count "$BRANCH"..master)"
  if [ "$local_behind_master" -gt 0 ]; then
    echo "== merging master into $BRANCH ($local_behind_master commits)"
    if ! git merge --no-edit master; then
      echo "" >&2
      echo "CONFLICT: resolve the merge, 'git commit', then re-run this script." >&2
      exit 4
    fi
    if [ "${NO_PUSH:-0}" != "1" ]; then
      git push origin "$BRANCH"
    fi
  else
    echo "== $BRANCH already contains master"
  fi
fi

if [ "${SKIP_TESTS:-0}" != "1" ]; then
  echo "== regression gates (cargo test -p jcode-base --lib)"
  cargo test -p jcode-base --lib
else
  echo "== skipping tests (SKIP_TESTS=1)"
fi

echo "== publishing self-dev build"
LAUNCHER="$(command -v jcode)"
LOG="$(mktemp /tmp/selfdev-build.XXXXXX)"
JCODE_REPO_DIR="$REPO" "$LAUNCHER" self-dev --build >"$LOG" 2>&1 &
BUILD_PID=$!

deadline=$((SECONDS + BUILD_TIMEOUT_S))
published=0
# The launcher exits right after publishing when it cannot open the canary
# TUI (non-interactive shells), so the marker must be checked even after the
# process is gone.
while :; do
  if grep -q "updated current launcher" "$LOG" 2>/dev/null; then
    published=1
    break
  fi
  if ! kill -0 "$BUILD_PID" 2>/dev/null; then
    break
  fi
  if ! grep -qE "Compiling|Building|Downloading|Blocking" "$LOG" 2>/dev/null \
      && grep -qE "^error|error\[|panicked|Failed" "$LOG" 2>/dev/null; then
    echo "BUILD FAILED:" >&2
    tail -40 "$LOG" >&2
    kill "$BUILD_PID" 2>/dev/null || true
    exit 5
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "TIMEOUT after ${BUILD_TIMEOUT_S}s:" >&2
    tail -40 "$LOG" >&2
    kill "$BUILD_PID" 2>/dev/null || true
    exit 5
  fi
  sleep 1
done

if [ "$published" != "1" ]; then
  echo "self-dev exited before publishing:" >&2
  tail -40 "$LOG" >&2
  exit 5
fi

echo "== published; stopping the canary launcher"
kill "$BUILD_PID" 2>/dev/null || true
wait "$BUILD_PID" 2>/dev/null || true
grep -E "Build complete|Starting self-dev session" "$LOG" || true

if [ "${CANARY:-0}" != "1" ]; then
  # The launcher spawns the canary TUI in a new terminal window; close the
  # freshly opened window again unless asked to keep it.
  osascript -e 'tell application "Terminal" to close (every window whose name contains "[self-dev]")' >/dev/null 2>&1 || true
fi

echo "== reloading shared daemon"
"$LAUNCHER" server reload --force || echo "WARN: server reload failed; daemon keeps the old build until restarted" >&2

sleep 3
DAEMON_PID="$(pgrep -f 'jcode.*serve' | head -1 || true)"
RUNNING="?"
if [ -n "$DAEMON_PID" ]; then
  RUNNING="$(lsof -p "$DAEMON_PID" 2>/dev/null | awk '$4=="txt" && /builds\/versions/ {print $NF; exit}')"
fi

echo ""
echo "== done"
LABEL="$(cat ~/.jcode/builds/current-version 2>/dev/null || echo '?')"
echo "   published : $LABEL"
echo "   daemon    : ${RUNNING:-not running}"
case "$RUNNING" in
  */versions/"$LABEL"/jcode) echo "   daemon is on the published build" ;;
  *) echo "   WARN: daemon is NOT on the published build" ;;
esac
