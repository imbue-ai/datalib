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

WORKSPACE="${BUILD_WORKSPACE_DIRECTORY:-}"
if [[ -z "$WORKSPACE" ]]; then
  echo "ERROR: BUILD_WORKSPACE_DIRECTORY not set; run via 'bazelisk run //frankweiler/ui:e2e'" >&2
  exit 1
fi
UI_DIR="$WORKSPACE/frankweiler/ui"
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

# Pass through any extra args (e.g. test names).
cd "$UI_DIR"
exec pnpm exec playwright test "$@"
