#!/usr/bin/env bash
# Run Playwright e2e tests against the bazel-built backend + fixture.
#
# Two modes, distinguished by `BUILD_WORKSPACE_DIRECTORY`:
#
#   * `bazel run //datalib/ui:e2e`  (BUILD_WORKSPACE_DIRECTORY set)
#       Interactive dev workflow. Uses the source-tree `datalib/ui/`
#       directly so spec edits round-trip without a rebuild. Requires a
#       working source-tree `node_modules` (run `pnpm install` once).
#
#   * `bazel test //datalib/ui:e2e_test`  (no BUILD_WORKSPACE_DIRECTORY)
#       Hermetic-ish: Playwright runs from the runfiles tree, against
#       the bazel-linked `:node_modules` (rules_js / pnpm-lock.yaml).
#       Independent of host `pnpm install` state. Browser binaries
#       (chromium + webkit) still come from
#       `~/Library/Caches/ms-playwright` via env_inherit=HOME.
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
  UI_DIR="$WORKSPACE/datalib/ui"
  [[ -d "$UI_DIR" ]] || { echo "ERROR: UI dir not found: $UI_DIR" >&2; exit 1; }

  # pnpm is pinned in datalib/ui/package.json's `packageManager` field
  # and provisioned on demand via corepack (ships with Node 16.9+). See
  # scripts/ensure_pnpm.sh for the bootstrap logic. UI_DIR is
  # `<workspace>/datalib/ui`, so `../../scripts/` is the workspace
  # scripts dir.
  # shellcheck source=../../scripts/ensure_pnpm.sh
  source "$UI_DIR/../../scripts/ensure_pnpm.sh"

  if [[ ! -d "$UI_DIR/node_modules" ]]; then
    (cd "$UI_DIR" && pnpm install)
  fi
  # Both engines: the suite has a `webkit` project so the specs that
  # render an AG Grid also run in the engine the Tauri desktop app
  # actually uses (WKWebView).
  (cd "$UI_DIR" && pnpm exec playwright install chromium webkit >/dev/null)
  PLAYWRIGHT_CMD=(pnpm exec playwright test)
else
  # ─── `bazel test` mode ───────────────────────────────────────────────
  # The runfile entry for our own package.json is the canonical anchor.
  # Resolving its parent directory in the runfiles tree gives us a
  # spot where node_modules (from :node_modules), playwright.config.ts,
  # tsconfig.json, and tests/e2e/ all sit side-by-side — exactly what
  # Playwright expects under `cwd`.
  PKG_RUNFILE="$(rlocation _main/datalib/ui/package.json)" || PKG_RUNFILE=""
  if [[ -z "$PKG_RUNFILE" || ! -e "$PKG_RUNFILE" ]]; then
    echo "ERROR: cannot locate datalib/ui/package.json in runfiles" >&2
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
  # Explicit `XXXXXX` template rather than `-t datalib-e2e-stage`: BSD mktemp
  # (macOS) treats `-t` as a prefix and tolerates a template with no X's,
  # but GNU mktemp (Linux/CI) reads the arg as a literal template and
  # aborts with "too few X's in template 'datalib-e2e-stage'". The full
  # `$TMPDIR/...XXXXXX` form is accepted identically by both.
  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/datalib-e2e-stage.XXXXXX")"
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
  PLAYWRIGHT_CLI="$STAGE_DIR/node_modules/@playwright/test/cli.js"

  # Browser binaries are not a bazel input — they come from the host's
  # ~/Library/Caches/ms-playwright via env_inherit=HOME (see the
  # hermeticity note in BUILD.bazel). Ask playwright to fetch anything
  # missing rather than failing deep inside a spec with
  # "Executable doesn't exist". It is a no-op once the revisions the
  # pinned playwright wants are cached, which is why the target carries
  # `requires-network`. `webkit` is the one the desktop app's WKWebView
  # matches; `chromium` covers the rest of the suite.
  node "$PLAYWRIGHT_CLI" install chromium webkit >/dev/null
  PLAYWRIGHT_CMD=(node "$PLAYWRIGHT_CLI" test)
fi

# Resolve the bazel-built backend binary from runfiles and export it for
# playwright.config.ts. Without this, playwright falls back to the
# source-workspace `bazel-bin/...` convenience symlink, which is not a
# declared input of this test and can race with concurrent bazel actions
# under `bazel test //...`.
BACKEND_BIN_RUNFILE="$(rlocation _main/datalib/backend/http/datalib_http_bin)" || BACKEND_BIN_RUNFILE=""
if [[ -n "$BACKEND_BIN_RUNFILE" && -x "$BACKEND_BIN_RUNFILE" ]]; then
  export DATALIB_HTTP_BIN="$BACKEND_BIN_RUNFILE"
fi

# Resolve the shared TNG materializer so playwright.config.ts can spawn
# it directly (same script as `bazelisk run //datalib:dev_tng`).
MATERIALIZE_RUNFILE="$(rlocation _main/tests/fixtures/materialize_tng_root)" || MATERIALIZE_RUNFILE=""
if [[ -n "$MATERIALIZE_RUNFILE" && -x "$MATERIALIZE_RUNFILE" ]]; then
  export FW_E2E_MATERIALIZE_TNG_ROOT="$MATERIALIZE_RUNFILE"
fi

# Resolve the step host so the sync spec can name it as a step's
# `command:`. The fixture data root is a temp dir with nothing on PATH,
# so an absolute path is the only way a step can be spawned there —
# same reason the materializer writes the applet's path absolutely.
STEP_BIN_RUNFILE="$(rlocation _main/datalib/backend/datalib_step/datalib_step)" || STEP_BIN_RUNFILE=""
if [[ -n "$STEP_BIN_RUNFILE" && -x "$STEP_BIN_RUNFILE" ]]; then
  export FW_E2E_DATALIB_STEP="$STEP_BIN_RUNFILE"
fi

# The DAG runner. The http server's sync worker resolves it from
# $DATALIB_DAG_BIN, then from its own directory, then PATH — and under
# `bazel test` it sits in the runfiles rather than beside the server, so
# the env var is the only one of the three that finds it.
DAG_BIN_RUNFILE="$(rlocation _main/datalib/backend/dag/datalib_dag_bin)" || DAG_BIN_RUNFILE=""
if [[ -n "$DAG_BIN_RUNFILE" && -x "$DAG_BIN_RUNFILE" ]]; then
  export DATALIB_DAG_BIN="$DAG_BIN_RUNFILE"
fi

# The local PDF corpus the sync spec scans. Anchor off one file and
# hand over its directory, the way materialize_tng_root.sh anchors the
# fsindex tree off its breadcrumb.
PDF_ANCHOR="$(rlocation _main/datalib/backend/etl/providers/pdf/tests/fixtures/pdf_tng/captains_log.pdf)" || PDF_ANCHOR=""
if [[ -n "$PDF_ANCHOR" && -f "$PDF_ANCHOR" ]]; then
  export FW_E2E_PDF_FIXTURE_DIR="$(dirname "$PDF_ANCHOR")"
fi

cd "$UI_DIR"
exec "${PLAYWRIGHT_CMD[@]}" "$@"
