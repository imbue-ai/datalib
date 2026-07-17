#!/usr/bin/env bash
# Launch the frankweiler HTTP backend and open a browser at it.
# Invoked via `bazelisk run //frankweiler:serve`.
set -eo pipefail

# --- bazel runfiles bootstrap ---
# https://github.com/bazelbuild/bazel/blob/master/tools/bash/runfiles/runfiles.bash
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

BIN="$(rlocation _main/frankweiler/backend/http/frankweiler_http_bin)"
[[ -x "$BIN" ]] || { echo "ERROR: backend binary not found at $BIN" >&2; exit 1; }

# The backend's sync worker shells out to the datalib-dag runner (which
# spawns datalib-step). Hand it the runfiles paths so UI-triggered
# "Sync" runs the real pipeline. Honor caller-supplied overrides.
if [[ -z "${FRANKWEILER_DAG_BIN:-}" ]]; then
  DAG_BIN="$(rlocation _main/frankweiler/backend/dag/datalib_dag || true)"
  [[ -x "$DAG_BIN" ]] && export FRANKWEILER_DAG_BIN="$DAG_BIN"
fi
if [[ -z "${FRANKWEILER_STEP_BIN:-}" ]]; then
  STEP_BIN="$(rlocation _main/frankweiler/backend/datalib_step/datalib_step || true)"
  [[ -x "$STEP_BIN" ]] && export FRANKWEILER_STEP_BIN="$STEP_BIN"
fi
[[ -n "${FRANKWEILER_DAG_BIN:-}" ]] && echo "dag bin: $FRANKWEILER_DAG_BIN"

# Default to an ephemeral port so concurrent `serve_dev.sh` runs (e.g. one
# agent per checkout) don't fight over a hardcoded 8731. Honor a caller-
# supplied FRANKWEILER_BIND verbatim. Same ephemeral-port trick as
# frankweiler/ui/playwright.config.ts — small race between close() and the
# binary's listen() but good enough for parallel local runs.
free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'
}
if [[ -z "${FRANKWEILER_BIND:-}" ]]; then
  FRANKWEILER_BIND="127.0.0.1:$(free_port)"
fi
export FRANKWEILER_BIND
echo "backend bind: $FRANKWEILER_BIND"

# FRANKWEILER_URL still wins if the caller set it explicitly (legacy
# override for "where should I open the browser / probe health?"). Otherwise
# derive from FRANKWEILER_BIND so the random port flows through.
BASE_URL="${FRANKWEILER_URL:-http://$FRANKWEILER_BIND}"
HEALTH_URL="$BASE_URL/api/health"

# Positional data-root arg required by the binary; default to
# ~/Documents/datalib if not supplied (legacy default).
if [[ $# -ge 1 && -n "$1" ]]; then
  ROOT_ARG="$1"
  case "$ROOT_ARG" in
    "~")     ROOT_ARG="$HOME" ;;
    "~/"*)   ROOT_ARG="$HOME/${ROOT_ARG#\~/}" ;;
  esac
else
  ROOT_ARG="$HOME/Documents/datalib"
fi
echo "data root: $ROOT_ARG"

# `--no-open` because this wrapper opens the URL itself (below) after
# waiting for the health endpoint to come up.
"$BIN" "$ROOT_ARG" --no-open &
BIN_PID=$!
trap 'kill "$BIN_PID" 2>/dev/null || true' EXIT INT TERM

for _ in 1 2 3 4 5 6 7 8 9 10; do
  if curl -sf "$HEALTH_URL" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

case "$(uname -s)" in
  Darwin) open "$BASE_URL" ;;
  Linux)  xdg-open "$BASE_URL" >/dev/null 2>&1 || true ;;
  *)      echo "open $BASE_URL in your browser" ;;
esac

wait "$BIN_PID"
