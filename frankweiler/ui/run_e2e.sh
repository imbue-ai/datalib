#!/usr/bin/env bash
# Run Playwright e2e tests against the bazel-built backend + fixture.
#
# Two modes, distinguished by `BUILD_WORKSPACE_DIRECTORY`:
#
#   * `bazel run //frankweiler/ui:e2e`  (BUILD_WORKSPACE_DIRECTORY set)
#       Interactive dev workflow. Uses the source-tree `frankweiler/ui/`
#       directly so spec edits round-trip without a rebuild. Requires a
#       working source-tree `node_modules` (run `pnpm install` once).
#
#   * `bazel test //frankweiler/ui:e2e_test`  (no BUILD_WORKSPACE_DIRECTORY)
#       Hermetic-ish: Playwright runs from the runfiles tree, against
#       the bazel-linked `:node_modules` (rules_js / pnpm-lock.yaml).
#       Independent of host `pnpm install` state. Chromium binary still
#       comes from `~/Library/Caches/ms-playwright` via env_inherit=HOME.
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

if [[ -n "$WORKSPACE" ]]; then
  # ─── `bazel run` mode ────────────────────────────────────────────────
  UI_DIR="$WORKSPACE/frankweiler/ui"
  [[ -d "$UI_DIR" ]] || { echo "ERROR: UI dir not found: $UI_DIR" >&2; exit 1; }

  # pnpm is pinned in frankweiler/ui/package.json's `packageManager` field
  # and provisioned on demand via corepack (ships with Node 16.9+). See
  # scripts/ensure_pnpm.sh for the bootstrap logic. UI_DIR is
  # `<workspace>/frankweiler/ui`, so `../../scripts/` is the workspace
  # scripts dir.
  # shellcheck source=../../scripts/ensure_pnpm.sh
  source "$UI_DIR/../../scripts/ensure_pnpm.sh"

  if [[ ! -d "$UI_DIR/node_modules" ]]; then
    (cd "$UI_DIR" && pnpm install)
  fi
  (cd "$UI_DIR" && pnpm exec playwright install chromium >/dev/null)
  PLAYWRIGHT_CMD=(pnpm exec playwright test)
else
  # ─── `bazel test` mode ───────────────────────────────────────────────
  # The runfile entry for our own package.json is the canonical anchor.
  # Resolving its parent directory in the runfiles tree gives us a
  # spot where node_modules (from :node_modules), playwright.config.ts,
  # tsconfig.json, and tests/e2e/ all sit side-by-side — exactly what
  # Playwright expects under `cwd`.
  PKG_RUNFILE="$(rlocation _main/frankweiler/ui/package.json)" || PKG_RUNFILE=""
  if [[ -z "$PKG_RUNFILE" || ! -e "$PKG_RUNFILE" ]]; then
    echo "ERROR: cannot locate frankweiler/ui/package.json in runfiles" >&2
    exit 1
  fi
  UI_DIR="$(dirname "$PKG_RUNFILE")"
  if [[ ! -d "$UI_DIR/node_modules" ]]; then
    echo "ERROR: bazel-linked node_modules not present at $UI_DIR/node_modules" >&2
    echo "Did :node_modules drop out of e2e_test's data?" >&2
    exit 1
  fi

  # The runfile dir contains symlinks back to source-tree files for
  # package.json / playwright.config.ts / tests/, plus a bazel-managed
  # `node_modules`. rules_js doesn't materialize the pnpm-style
  # `node_modules/.bin/` shims — packages live at their canonical
  # `node_modules/<scope>/<name>/` path. Invoke the playwright cli
  # JavaScript directly via `node`.
  PLAYWRIGHT_CLI="$UI_DIR/node_modules/@playwright/test/cli.js"
  if [[ ! -f "$PLAYWRIGHT_CLI" ]]; then
    echo "ERROR: playwright cli not found at $PLAYWRIGHT_CLI" >&2
    exit 1
  fi

  # Playwright's spec-file walker skips symlinks (Node's `fs.readdir`
  # with `withFileTypes:true` reports a symlink-to-file as
  # `!isFile()`, so the walker excludes it — "Error: No tests found"
  # even though the targets resolve fine for `node` itself). The
  # runfiles tree has the specs and configs as symlinks back to
  # bazel-out / source, so we rehome the test inputs into a tempdir
  # as real files (rsync -L resolves symlinks during the copy).
  # Explicit `XXXXXX` template rather than `-t fw-e2e-stage`: BSD mktemp
  # (macOS) treats `-t` as a prefix and tolerates a template with no X's,
  # but GNU mktemp (Linux/CI) reads the arg as a literal template and
  # aborts with "too few X's in template 'fw-e2e-stage'". The full
  # `$TMPDIR/...XXXXXX` form is accepted identically by both.
  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/fw-e2e-stage.XXXXXX")"
  trap 'rm -rf "$STAGE_DIR"' EXIT
  rsync -aL \
    --exclude node_modules \
    --exclude e2e_test \
    --exclude run_e2e.sh \
    --exclude test-results \
    "$UI_DIR/" "$STAGE_DIR/"
  # node_modules has to stay where rules_js put it (its packages
  # reference each other by relative path across `node_modules`),
  # so we link it in rather than copy.
  ln -s "$UI_DIR/node_modules" "$STAGE_DIR/node_modules"
  UI_DIR="$STAGE_DIR"
  PLAYWRIGHT_CMD=(node "$STAGE_DIR/node_modules/@playwright/test/cli.js" test)
fi

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

cd "$UI_DIR"
exec "${PLAYWRIGHT_CMD[@]}" "$@"
