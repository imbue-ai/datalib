#!/usr/bin/env bash
# Launch backend (frankweiler_http_bin) AND `pnpm dev` (Vite), wait for Vite to
# bind, then open the browser at the Vite URL.
#
# Invoked via `bazelisk run //frankweiler:dev`.
#
# Configuration:
#   FRANKWEILER_ROOT  data root (default: ~/Documents/personal-mirror or
#                     whatever ~/.config/frankweiler/config.yaml says)
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

# Ensure UI deps are installed.
if [[ ! -d "$UI_DIR/node_modules" ]]; then
  echo "first run: installing UI deps via pnpm…"
  (cd "$UI_DIR" && pnpm install)
fi

# Start the backend.
"$BIN" &
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
