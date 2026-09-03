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

# Temp dirs this script mints, removed by one EXIT trap. Several of them
# and only one `trap ... EXIT` slot, so they are named here rather than
# each installing a handler that would silently replace the other's.
STAGE_DIR=""
BIN_STAGE=""
RUNTIME_STAGE=""
cleanup() {
  [[ -n "$STAGE_DIR" ]] && rm -rf "$STAGE_DIR"
  [[ -n "$BIN_STAGE" ]] && rm -rf "$BIN_STAGE"
  [[ -n "$RUNTIME_STAGE" ]] && rm -rf "$RUNTIME_STAGE"
  return 0
}
trap cleanup EXIT

# Which browser engines to provision. Both by default: the suite has a
# `webkit` project so the AG-Grid specs also run in the engine the Tauri
# desktop app uses (WKWebView).
#
# Overridable because the Linux CI image bakes Chromium *and its OS
# libraries* but not WebKit's — see `.devcontainer/Dockerfile`. There,
# `install webkit` would download ~100 MB of browser that cannot launch
# for want of shared libraries, so a chromium-only run says so up front
# rather than paying for a download it will not use.
E2E_BROWSERS="${E2E_BROWSERS:-chromium webkit}"

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
  # shellcheck disable=SC2086  # E2E_BROWSERS is a deliberate word list
  (cd "$UI_DIR" && pnpm exec playwright install $E2E_BROWSERS >/dev/null)
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
  # matches; `chromium` covers the rest of the suite. `E2E_BROWSERS`
  # narrows it — see the note where it is defined.
  # shellcheck disable=SC2086  # E2E_BROWSERS is a deliberate word list
  node "$PLAYWRIGHT_CLI" install $E2E_BROWSERS >/dev/null
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

# A directory holding every shipped binary under its **public
# dash-separated name** — the layout `scripts/install.sh` produces on a
# user's machine, and the one `//datalib/backend:bin` builds.
#
# The onboarding spec needs it, and nothing else here does. That spec
# starts from an empty data root and clicks the button that writes the
# starter config, so its config is the scaffold `datalib-http` emits:
# bare `datalib-step …` / `datalib-applet …` commands, resolved against
# PATH. Every other spec's config names an absolute runfiles path
# instead, which is why they have never needed this. Symlinks rather
# than a copy_to_directory dep: bazel names each output after its target
# (`datalib_step`, `datalib_dag_bin`), and the rename is the whole point.
BIN_STAGE="$(mktemp -d "${TMPDIR:-/tmp}/datalib-e2e-bin.XXXXXX")"
APPLET_BIN_RUNFILE="$(rlocation _main/datalib/backend/applets/datalib_applet)" || APPLET_BIN_RUNFILE=""
for pair in \
  "datalib-step:${STEP_BIN_RUNFILE:-}" \
  "datalib-dag:${DAG_BIN_RUNFILE:-}" \
  "datalib-applet:${APPLET_BIN_RUNFILE:-}"; do
  public="${pair%%:*}"
  built="${pair#*:}"
  if [[ -n "$built" && -x "$built" ]]; then
    ln -sfn "$built" "$BIN_STAGE/$public"
  fi
done
export FW_E2E_BIN_DIR="$BIN_STAGE"

# The local PDF corpus the sync spec scans. Anchor off one file and
# hand over its directory, the way materialize_tng_root.sh anchors the
# fsindex tree off its breadcrumb.
PDF_ANCHOR="$(rlocation _main/datalib/backend/etl/providers/pdf/tests/fixtures/pdf_tng/captains_log.pdf)" || PDF_ANCHOR=""
if [[ -n "$PDF_ANCHOR" && -f "$PDF_ANCHOR" ]]; then
  export FW_E2E_PDF_FIXTURE_DIR="$(dirname "$PDF_ANCHOR")"
fi

# Signal, for the onboarding spec's second source. Unlike the PDF
# corpus there is nothing to point at directly: a Signal backup is an
# encrypted blob, so it is *generated* from a checked-in JSON spec.
# Hand playwright.config.ts both halves and let it expand them once per
# config load, next to where it seeds the PDF scan directory.
SIGNAL_FIXTURE_BIN="$(rlocation _main/datalib/backend/signal-backup/signal_make_fixture)" || SIGNAL_FIXTURE_BIN=""
if [[ -n "$SIGNAL_FIXTURE_BIN" && -x "$SIGNAL_FIXTURE_BIN" ]]; then
  export FW_E2E_SIGNAL_MAKE_FIXTURE="$SIGNAL_FIXTURE_BIN"
fi
SIGNAL_SPEC="$(rlocation _main/datalib/backend/etl/providers/signal/tests/fixtures/signal_tng/tng.json)" || SIGNAL_SPEC=""
if [[ -n "$SIGNAL_SPEC" && -f "$SIGNAL_SPEC" ]]; then
  export FW_E2E_SIGNAL_SPEC="$SIGNAL_SPEC"
fi

# --- the Node that runs qmd ------------------------------------------
#
# Stage a `DATALIB_RUNTIME_DIR` from the Bazel-managed Node and qmd
# package tree, the same two symlinks `tests/fixtures/build_qmd_index.py`
# makes. With it set, `datalib_core::node_runtime::bundled_command` wins
# and the `npx -y @tobilu/qmd@<v>` fallback is never reached.
#
# Why it matters here specifically: npm keys the npx cache on the package
# spec alone (`~/.npm/_npx/<hash-of-@tobilu/qmd@<version>>`), with no Node
# version in the key, so every Node on the machine shares one directory —
# and back when this raced, the better-sqlite3 binding installed into it
# was built for a single ABI. This test inherits the developer's PATH while every other Bazel
# action uses the pinned PATH from `.bazelrc`, so the two raced to
# populate that directory and whichever lost died with
# "NODE_MODULE_VERSION 147 ... requires 127". Pinning the Node removes
# the race rather than papering over it.
#
# Deliberately fatal on a miss. A silent fall-through to npx is exactly
# the bug this block exists to delete, and it would look like a pass.
NODE_BIN_RUNFILE="$(rlocation "${FW_E2E_NODE_BIN_RLOC:-}")" || NODE_BIN_RUNFILE=""
QMD_PKG_RUNFILE="$(rlocation "${FW_E2E_QMD_PKG_RLOC:-}")" || QMD_PKG_RUNFILE=""
if [[ ! -x "$NODE_BIN_RUNFILE" ]]; then
  echo "ERROR: bazel-managed node not in runfiles (FW_E2E_NODE_BIN_RLOC='${FW_E2E_NODE_BIN_RLOC:-}')" >&2
  echo "Did @nodejs_host//:node_bin drop out of _E2E_DATA / _E2E_ENV?" >&2
  exit 1
fi
if [[ ! -f "$QMD_PKG_RUNFILE" ]]; then
  echo "ERROR: qmd runtime package.json not in runfiles (FW_E2E_QMD_PKG_RLOC='${FW_E2E_QMD_PKG_RLOC:-}')" >&2
  exit 1
fi

# The version names the staged directory, and `bundled_command` looks it
# up by the Rust constant — so read it from the package.json that
# //tools:version_pins_test already holds equal to DEFAULT_QMD_VERSION
# rather than writing the number down a seventh time.
QMD_VERSION="$(sed -n 's/.*"@tobilu\/qmd"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$QMD_PKG_RUNFILE")"
if [[ -z "$QMD_VERSION" ]]; then
  echo "ERROR: no @tobilu/qmd version in $QMD_PKG_RUNFILE" >&2
  exit 1
fi

# The whole package store sits beside that package.json in the runfiles
# tree; qmd resolves its deps from siblings inside it, which is why
# `:qmd_tree` (the entire store) is the data dep rather than the single
# `@tobilu/qmd` link.
QMD_STORE="$(dirname "$QMD_PKG_RUNFILE")/node_modules"
if [[ ! -d "$QMD_STORE" ]]; then
  echo "ERROR: qmd package store not in runfiles at $QMD_STORE" >&2
  echo "Did //third-party/qmd/runtime:qmd_tree drop out of _E2E_DATA?" >&2
  exit 1
fi

RUNTIME_STAGE="$(mktemp -d "${TMPDIR:-/tmp}/datalib-e2e-runtime.XXXXXX")"
mkdir -p "$RUNTIME_STAGE/node/bin" "$RUNTIME_STAGE/qmd/$QMD_VERSION"
ln -sfn "$NODE_BIN_RUNFILE" "$RUNTIME_STAGE/node/bin/node"
ln -sfn "$QMD_STORE" "$RUNTIME_STAGE/qmd/$QMD_VERSION/node_modules"

# Assert the entry `bundled_command` will look for actually resolves.
# Without this the staging can be subtly wrong (bad version, moved store)
# and the only symptom is qmd quietly running from npx again.
QMD_ENTRY="$RUNTIME_STAGE/qmd/$QMD_VERSION/node_modules/@tobilu/qmd/dist/cli/qmd.js"
if [[ ! -f "$QMD_ENTRY" ]]; then
  echo "ERROR: staged qmd entry missing: $QMD_ENTRY" >&2
  echo "The runtime tree is wrong, and qmd would silently fall back to npx." >&2
  exit 1
fi
export DATALIB_RUNTIME_DIR="$RUNTIME_STAGE"

cd "$UI_DIR"
exec "${PLAYWRIGHT_CMD[@]}" "$@"
