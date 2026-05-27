#!/usr/bin/env bash
# Launch backend (frankweiler_http_bin) AND `pnpm dev` (Vite), wait for Vite to
# bind, then open the browser at the Vite URL.
#
# Invoked via `bazelisk run //frankweiler:dev`.
#
# Configuration:
#   $1 (positional)   data root. Leading tildes expanded relative to
#                     $HOME. Defaults to ~/Documents/mixed-up-files when
#                     not given.
#                     e.g. `bazelisk run //frankweiler:dev -- ~/mixed_up_files.thad`
#   FRANKWEILER_PORT  Vite port (default: 5173)
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

BIN="$(rlocation _main/frankweiler/backend/http/frankweiler_http_bin)"
[[ -x "$BIN" ]] || { echo "ERROR: backend binary not found at $BIN" >&2; exit 1; }

WORKSPACE="${BUILD_WORKSPACE_DIRECTORY:-}"
if [[ -z "$WORKSPACE" ]]; then
  echo "ERROR: BUILD_WORKSPACE_DIRECTORY not set; run via 'bazelisk run //frankweiler:dev'" >&2
  exit 1
fi
UI_DIR="$WORKSPACE/frankweiler/ui"
[[ -d "$UI_DIR" ]] || { echo "ERROR: UI dir not found: $UI_DIR" >&2; exit 1; }

if ! command -v pnpm >/dev/null 2>&1; then
  echo "ERROR: pnpm not on PATH. Install with 'npm install -g pnpm' or via brew." >&2
  exit 1
fi

PORT="${FRANKWEILER_PORT:-5173}"

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
  ROOT_ARG="$HOME/Documents/mixed-up-files"
fi
echo "data root: $ROOT_ARG"

# pnpm pinned via frankweiler/ui/package.json's `packageManager`;
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
  echo "ERROR: port $PORT is already in use. Stop the other process or set FRANKWEILER_PORT." >&2
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
