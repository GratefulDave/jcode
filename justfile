# jcode fork maintenance recipes.
#
#   just up                # upstream sync → regression gates → publish → reload
#   just up stages="sync"  # any subset: sync test publish
#   just sync              # upstream sync only
#   just test              # jcode-base regression gates
#
# Env knobs (override per invocation, e.g. `just CANARY=1 up`):
#   UPSTREAM_REMOTE  git remote to pull from        (default: upstream)
#   SKIP_TESTS       skip gates in `up`             (default: 0)
#   NO_PUSH          don't push master/branch       (default: 0)
#   CANARY           keep the canary TUI window     (default: 0)
#   BUILD_TIMEOUT_S  publish timeout in seconds     (default: 1800)

set positional-arguments

export UPSTREAM_REMOTE := env_var_or_default("UPSTREAM_REMOTE", "upstream")
export SKIP_TESTS := env_var_or_default("SKIP_TESTS", "0")
export NO_PUSH := env_var_or_default("NO_PUSH", "0")
export CANARY := env_var_or_default("CANARY", "0")
export BUILD_TIMEOUT_S := env_var_or_default("BUILD_TIMEOUT_S", "1800")

default:
    @just --list

# full loop: upstream sync → gates → publish → daemon reload
@up stages="sync test publish":
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ justfile_directory() }}"
    has_stage() { [[ " {{ stages }} " == *" $1 "* ]]; }

    if has_stage sync; then
      echo "== fetching $UPSTREAM_REMOTE"
      git fetch "$UPSTREAM_REMOTE" --prune
      UP_MASTER="$(git rev-parse --verify --quiet "$UPSTREAM_REMOTE/master")" \
        || { echo "$UPSTREAM_REMOTE/master not found" >&2; exit 3; }
      BRANCH="$(git rev-parse --abbrev-ref HEAD)"

      sync_master() {
        if git merge-base --is-ancestor "$UP_MASTER" master; then
          echo "== master already contains $UPSTREAM_REMOTE/master"
          return
        fi
        git checkout master
        if git merge-base --is-ancestor master "$UP_MASTER"; then
          echo "== fast-forwarding master ($(git rev-list --count master.."$UP_MASTER") commits)"
          git merge --ff-only "$UP_MASTER"
          [ "$NO_PUSH" = "1" ] || git push origin master
        else
          echo "== master diverged from $UPSTREAM_REMOTE; rebasing"
          if ! git rebase "$UP_MASTER" master; then
            echo "REBASE CONFLICT on master: resolve, 'git rebase --continue', re-run." >&2
            exit 4
          fi
          [ "$NO_PUSH" = "1" ] || git push origin master --force-with-lease
        fi
      }

      if [ "$BRANCH" = "master" ]; then
        sync_master
      else
        sync_master
        git checkout "$BRANCH"
        behind="$(git rev-list --count "$BRANCH"..master)"
        if [ "$behind" -gt 0 ]; then
          echo "== merging master into $BRANCH ($behind commits)"
          if ! git merge --no-edit master; then
            echo "CONFLICT: resolve the merge, 'git commit', then re-run." >&2
            exit 4
          fi
          [ "$NO_PUSH" = "1" ] || git push origin "$BRANCH"
        else
          echo "== $BRANCH already contains master"
        fi
      fi
    fi

    if has_stage test; then
      echo "== regression gates (cargo test -p jcode-base --lib)"
      cargo test -p jcode-base --lib
    else
      echo "== skipping tests"
    fi

    if has_stage publish; then
      echo "== publishing self-dev build"
      LOG="$(mktemp /tmp/selfdev-build.XXXXXX)"
      LAUNCHER="$(command -v jcode)"
      JCODE_REPO_DIR="$PWD" "$LAUNCHER" self-dev --build >"$LOG" 2>&1 &
      BUILD_PID=$!
      deadline=$((SECONDS + BUILD_TIMEOUT_S))
      published=0
      # The launcher exits right after publishing when it cannot open the
      # canary TUI (non-interactive shells), so check the marker even after
      # the process is gone.
      while :; do
        grep -q "updated current launcher" "$LOG" 2>/dev/null && { published=1; break; }
        kill -0 "$BUILD_PID" 2>/dev/null || break
        if ! grep -qE "Compiling|Building|Downloading|Blocking" "$LOG" 2>/dev/null \
            && grep -qE "^error|error\[|panicked|Failed" "$LOG" 2>/dev/null; then
          echo "BUILD FAILED:" >&2; tail -40 "$LOG" >&2
          kill "$BUILD_PID" 2>/dev/null || true
          exit 5
        fi
        [ "$SECONDS" -lt "$deadline" ] || {
          echo "TIMEOUT after ${BUILD_TIMEOUT_S}s:" >&2; tail -40 "$LOG" >&2
          kill "$BUILD_PID" 2>/dev/null || true
          exit 5
        }
        sleep 1
      done
      [ "$published" = "1" ] || { echo "self-dev exited before publishing:" >&2; tail -40 "$LOG" >&2; exit 5; }

      echo "== published; stopping the canary launcher"
      kill "$BUILD_PID" 2>/dev/null || true
      wait "$BUILD_PID" 2>/dev/null || true
      if [ "$CANARY" != "1" ]; then
        osascript -e 'tell application "Terminal" to close (every window whose name contains "[self-dev]")' >/dev/null 2>&1 || true
      fi

      echo "== reloading shared daemon"
      "$LAUNCHER" server reload --force || echo "WARN: server reload failed; daemon keeps the old build until restarted" >&2

      sleep 3
      DAEMON_PID="$(pgrep -f 'jcode.*serve' | head -1 || true)"
      RUNNING=""
      [ -n "$DAEMON_PID" ] && RUNNING="$(lsof -p "$DAEMON_PID" 2>/dev/null | awk '$4=="txt" && /builds\/versions/ {print $NF; exit}')"
      LABEL="$(cat ~/.jcode/builds/current-version 2>/dev/null || echo '?')"
      echo "== done"
      echo "   published : $LABEL"
      echo "   daemon    : ${RUNNING:-not running}"
      case "$RUNNING" in
        */versions/"$LABEL"/jcode) echo "   daemon is on the published build" ;;
        *) echo "   WARN: daemon is NOT on the published build" ;;
      esac
    fi

# upstream sync only (no gates, no build)
@sync stages="sync":
    @just up {{ stages }}

# regression gates only
@test:
    cargo test -p jcode-base --lib

# publish current tree and reload the daemon (no sync, no gates)
@publish stages="publish":
    @just up {{ stages }}
