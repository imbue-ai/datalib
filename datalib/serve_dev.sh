#!/usr/bin/env bash
# Launch the datalib HTTP backend and open a browser at it.
# Invoked via `bazelisk run //datalib:serve`.
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

BIN="$(rlocation _main/datalib/backend/http/datalib_http_bin)"
[[ -x "$BIN" ]] || { echo "ERROR: backend binary not found at $BIN" >&2; exit 1; }

# The backend's sync worker shells out to the datalib-dag runner (which
# spawns datalib-step via PATH). Hand it //datalib/backend:bin, which
# stages every shipped binary under its public dash-separated name in
# one directory, so UI-triggered "Sync" runs the real pipeline. Honor
# caller-supplied overrides.
BIN_DIR="$(rlocation _main/datalib/backend/bin || true)"
if [[ -d "$BIN_DIR" ]]; then
  : "${DATALIB_DAG_BIN:=$BIN_DIR/datalib-dag}"
  : "${DATALIB_BINARY_DIR:=$BIN_DIR}"
  export DATALIB_DAG_BIN DATALIB_BINARY_DIR
fi
[[ -n "${DATALIB_DAG_BIN:-}" ]] && echo "dag bin: $DATALIB_DAG_BIN"

# Default to an ephemeral port so concurrent `serve_dev.sh` runs (e.g. one
# agent per checkout) don't fight over a hardcoded 8731. Honor a caller-
# supplied DATALIB_BIND verbatim. Same ephemeral-port trick as
# datalib/ui/playwright.config.ts — small race between close() and the
# binary's listen() but good enough for parallel local runs.
free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'
}
if [[ -z "${DATALIB_BIND:-}" ]]; then
  DATALIB_BIND="127.0.0.1:$(free_port)"
fi
export DATALIB_BIND

# Every route behind the backend requires the per-process API token
# (datalib/backend/http/src/auth.rs). Pin one here so this wrapper can
# probe /api/health and open a browser URL that carries it; without
# DATALIB_TOKEN the binary would mint its own and we'd have to scrape
# stderr for it.
if [[ -z "${DATALIB_TOKEN:-}" ]]; then
  DATALIB_TOKEN="$(python3 -c 'import secrets;print(secrets.token_hex(32))')"
fi
export DATALIB_TOKEN
echo "backend bind: $DATALIB_BIND"

# DATALIB_URL still wins if the caller set it explicitly (legacy
# override for "where should I open the browser / probe health?"). Otherwise
# derive from DATALIB_BIND so the random port flows through.
BASE_URL="${DATALIB_URL:-http://$DATALIB_BIND}"
HEALTH_URL="$BASE_URL/api/health?token=$DATALIB_TOKEN"
# The browser trades ?token= for a session cookie on this first load and
# is redirected to the clean URL; every later request rides the cookie.
OPEN_URL="$BASE_URL/?token=$DATALIB_TOKEN"

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
  Darwin) open "$OPEN_URL" ;;
  Linux)  xdg-open "$OPEN_URL" >/dev/null 2>&1 || true ;;
  *)      echo "open $OPEN_URL in your browser" ;;
esac

wait "$BIN_PID"
