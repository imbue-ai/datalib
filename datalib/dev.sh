#!/usr/bin/env bash
# Launch backend (datalib_http_bin) AND `pnpm dev` (Vite), wait for Vite to
# bind, then open the browser at the Vite URL.
#
# Invoked via `bazelisk run //datalib:dev`.
#
# Configuration:
#   $1 (positional)     data root. Leading tildes expanded relative to
#                       $HOME. Defaults to ~/Documents/datalib when
#                       not given.
#                       e.g. `bazelisk run //datalib:dev -- ~/datalib.thad`
#   DATALIB_PORT    Vite port (default: ephemeral, freshly allocated)
#   DATALIB_BIND    Backend bind addr (default: 127.0.0.1:<ephemeral>)
#   DATALIB_BACKEND Vite proxy target for /api (default: derived from
#                       DATALIB_BIND)
#   DATALIB_TOKEN   Backend API token, shared with the Vite proxy
#                       (default: freshly minted per run)
#
# Both ports default to ephemeral so multiple concurrent agents/devs can
# each `bazelisk run //datalib:dev` from their own checkouts without
# colliding on a fixed port. Set the env vars to pin specific ports.
#
# Bazel sets BUILD_WORKSPACE_DIRECTORY when invoked via `bazel run`; we use it
# to locate the (un-bazeled) Vite UI source tree.

set -eo pipefail

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

# Sync worker child binaries (see serve_dev.sh for the rationale;
# short version: bazel names the step binary `datalib_step`, but step
# commands look up `datalib-step` on PATH, so we stage a dash-named
# symlink dir and hand it over as the binary dir).
if [[ -z "${DATALIB_DAG_BIN:-}" ]]; then
  DAG_BIN="$(rlocation _main/datalib/backend/dag/datalib_dag_bin || true)"
  [[ -x "$DAG_BIN" ]] && export DATALIB_DAG_BIN="$DAG_BIN"
fi
BINDIR=""
if [[ -z "${DATALIB_BINARY_DIR:-}" ]]; then
  STEP_BIN="$(rlocation _main/datalib/backend/datalib_step/datalib_step || true)"
  if [[ -x "$STEP_BIN" ]]; then
    BINDIR="$(mktemp -d -t datalib-bindir.XXXXXX)"
    ln -s "$STEP_BIN" "$BINDIR/datalib-step"
    export DATALIB_BINARY_DIR="$BINDIR"
  fi
fi
[[ -n "${DATALIB_DAG_BIN:-}" ]] && echo "dag bin: $DATALIB_DAG_BIN"

WORKSPACE="${BUILD_WORKSPACE_DIRECTORY:-}"
if [[ -z "$WORKSPACE" ]]; then
  echo "ERROR: BUILD_WORKSPACE_DIRECTORY not set; run via 'bazelisk run //datalib:dev'" >&2
  exit 1
fi
UI_DIR="$WORKSPACE/datalib/ui"
[[ -d "$UI_DIR" ]] || { echo "ERROR: UI dir not found: $UI_DIR" >&2; exit 1; }

if ! command -v pnpm >/dev/null 2>&1; then
  echo "ERROR: pnpm not on PATH. Install with 'npm install -g pnpm' or via brew." >&2
  exit 1
fi

# Default to ephemeral ports for both Vite and the backend so multiple
# concurrent dev runs don't collide. Same ephemeral-port trick as
# datalib/ui/playwright.config.ts: bind to :0, read back, close. Small
# race between close() and the real listener's bind() — fine in practice.
free_port() {
  python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'
}
PORT="${DATALIB_PORT:-$(free_port)}"
if [[ -z "${DATALIB_BIND:-}" ]]; then
  DATALIB_BIND="127.0.0.1:$(free_port)"
fi
export DATALIB_BIND
# Vite's /api proxy needs to point at whatever port the backend actually
# bound. Honor a caller-supplied DATALIB_BACKEND; otherwise derive
# from DATALIB_BIND so a random backend port flows through.
export DATALIB_BACKEND="${DATALIB_BACKEND:-http://$DATALIB_BIND}"
# The backend requires its API token on every route
# (datalib/backend/http/src/auth.rs). In this split-origin dev setup the
# browser loads the app from Vite, so it never gets the backend's
# session cookie — instead Vite's *server-side* /api proxy stamps the
# token onto each forwarded request (see vite.config.ts). Both processes
# therefore need the same token, so mint it here rather than letting the
# binary pick its own.
if [[ -z "${DATALIB_TOKEN:-}" ]]; then
  DATALIB_TOKEN="$(python3 -c 'import secrets;print(secrets.token_hex(32))')"
fi
export DATALIB_TOKEN
echo "vite port:     $PORT"
echo "backend bind:  $DATALIB_BIND"
echo "vite → /api:   $DATALIB_BACKEND"

# Positional data-root arg passed through to the backend binary. Manual
# tilde expansion: shells handle ~ in unquoted args, but if someone
# double-quotes the argument we still honor a leading ~/ or bare ~.
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

# pnpm pinned via datalib/ui/package.json's `packageManager`;
# provisioned on demand via corepack. See scripts/ensure_pnpm.sh.
# shellcheck source=../scripts/ensure_pnpm.sh
source "$WORKSPACE/scripts/ensure_pnpm.sh"

# Ensure UI deps are installed.
if [[ ! -d "$UI_DIR/node_modules" ]]; then
  echo "first run: installing UI deps via pnpm…"
  (cd "$UI_DIR" && pnpm install)
fi

# Bail early if the Vite port is already taken — otherwise pnpm dev will
# crash a few seconds in and we'd open a browser tab pointed at whatever
# *other* server is on that port. lsof is on every macOS / typical Linux.
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "ERROR: port $PORT is already in use. Stop the other process or set DATALIB_PORT." >&2
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >&2 || true
  exit 1
fi

# Start the backend. `--no-open` because this script opens the Vite
# URL (not the backend's bundled UI) itself after waiting for Vite to
# bind — letting the backend also auto-open would spawn a duplicate
# tab pointed at the static SPA.
"$BIN" "$ROOT_ARG" --no-open &
BACKEND_PID=$!

# Start Vite (foreground-stdio, but in the background process-wise).
(cd "$UI_DIR" && pnpm dev --port "$PORT") &
VITE_PID=$!

cleanup() {
  kill "$VITE_PID" 2>/dev/null || true
  kill "$BACKEND_PID" 2>/dev/null || true
  [[ -n "$BINDIR" ]] && rm -rf "$BINDIR"
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

URL="http://127.0.0.1:${PORT}/"

# Wait up to ~10s for Vite to bind.
for _ in $(seq 1 50); do
  if curl -sf "$URL" >/dev/null 2>&1; then break; fi
  if ! kill -0 "$VITE_PID" 2>/dev/null; then
    echo "ERROR: vite exited before binding" >&2
    exit 1
  fi
  sleep 0.2
done

case "$(uname -s)" in
  Darwin) open "$URL" ;;
  Linux)  xdg-open "$URL" >/dev/null 2>&1 || true ;;
  *)      echo "open $URL in your browser" ;;
esac

# Block on whichever child exits first; trap handles teardown.
wait -n "$BACKEND_PID" "$VITE_PID"
