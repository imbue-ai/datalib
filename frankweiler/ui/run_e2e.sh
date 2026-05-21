#!/usr/bin/env bash
# Run Playwright e2e tests against the bazel-built backend + fixture.
# Invoked via `bazelisk run //frankweiler/ui:e2e`.
set -eo pipefail

# --- bazel runfiles bootstrap ---
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

# Resolve the source UI directory:
#  - `bazel run`:  BUILD_WORKSPACE_DIRECTORY is set, use it.
#  - `bazel test`: runfiles contain a symlink back to the source
#                  package.json. Resolve it to the source path so we
#                  share the host's node_modules + browser cache.
WORKSPACE="${BUILD_WORKSPACE_DIRECTORY:-}"
if [[ -n "$WORKSPACE" ]]; then
  UI_DIR="$WORKSPACE/frankweiler/ui"
else
  PKG_RUNFILE="$(rlocation _main/frankweiler/ui/package.json)" || PKG_RUNFILE=""
  if [[ -z "$PKG_RUNFILE" || ! -e "$PKG_RUNFILE" ]]; then
    echo "ERROR: cannot locate frankweiler/ui/package.json in runfiles" >&2
    exit 1
  fi
  # The runfile entry itself is a symlink back to the source-tree
  # package.json. Resolve through the symlink (BSD `readlink` lacks -f,
  # so use python). dirname of the resolved path is the source UI dir.
  PKG_REAL="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$PKG_RUNFILE")"
  UI_DIR="$(dirname "$PKG_REAL")"
fi
[[ -d "$UI_DIR" ]] || { echo "ERROR: UI dir not found: $UI_DIR" >&2; exit 1; }

if ! command -v pnpm >/dev/null 2>&1; then
  echo "ERROR: pnpm not on PATH. Install via 'brew install pnpm'." >&2
  exit 1
fi

if [[ ! -d "$UI_DIR/node_modules" ]]; then
  (cd "$UI_DIR" && pnpm install)
fi

# Make sure Playwright's chromium is installed. This is a no-op once cached.
(cd "$UI_DIR" && pnpm exec playwright install chromium >/dev/null)

# Resolve the bazel-built backend binary from runfiles and export it for
# playwright.config.ts. Without this, playwright falls back to the
# source-workspace `bazel-bin/...` convenience symlink, which is not a
# declared input of this test and can race with concurrent bazel actions
# under `bazel test //...`.
BACKEND_BIN_RUNFILE="$(rlocation _main/frankweiler/backend/http/frankweiler_http_bin)" || BACKEND_BIN_RUNFILE=""
if [[ -n "$BACKEND_BIN_RUNFILE" && -x "$BACKEND_BIN_RUNFILE" ]]; then
  export FRANKWEILER_HTTP_BIN="$BACKEND_BIN_RUNFILE"
fi

# Resolve the shared TNG materializer so playwright.config.ts can spawn
# it directly (same script as `bazelisk run //frankweiler:dev_tng`).
MATERIALIZE_RUNFILE="$(rlocation _main/tests/fixtures/materialize_tng_root)" || MATERIALIZE_RUNFILE=""
if [[ -n "$MATERIALIZE_RUNFILE" && -x "$MATERIALIZE_RUNFILE" ]]; then
  export FW_E2E_MATERIALIZE_TNG_ROOT="$MATERIALIZE_RUNFILE"
fi

# Pass through any extra args (e.g. test names).
cd "$UI_DIR"
exec pnpm exec playwright test "$@"
