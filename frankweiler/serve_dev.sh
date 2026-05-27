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

BASE_URL="${FRANKWEILER_URL:-http://127.0.0.1:8731}"
HEALTH_URL="$BASE_URL/api/health"

# Positional data-root arg required by the binary; default to
# ~/Documents/mixed-up-files if not supplied (legacy default).
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
