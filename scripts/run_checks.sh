#!/usr/bin/env bash
# Read-only mirror of scripts/pre-commit, suitable for `bazel test`.
# Same checks (ruff, pyright, vue-tsc, cargo fmt, cargo clippy) but
# never mutates the working tree (no auto-format, no `git add`).
#
# Two modes:
#   * `bazel run  //:precommit`           — uses BUILD_WORKSPACE_DIRECTORY
#   * `bazel test //:precommit_test`      — resolves the source repo via
#                                           the pyproject.toml runfile
#                                           (the script itself runs in
#                                           the source tree, not the
#                                           sandbox, so it can see
#                                           .venv / node_modules / .git).
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
  PYPROJECT_RUNFILE="$(rlocation _main/pyproject.toml)" || PYPROJECT_RUNFILE=""
  if [[ -z "$PYPROJECT_RUNFILE" || ! -e "$PYPROJECT_RUNFILE" ]]; then
    echo "ERROR: cannot locate pyproject.toml in runfiles" >&2
    exit 1
  fi
  PYPROJECT_REAL="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$PYPROJECT_RUNFILE")"
  WORKSPACE="$(dirname "$PYPROJECT_REAL")"
fi
[[ -d "$WORKSPACE" ]] || { echo "ERROR: workspace not found: $WORKSPACE" >&2; exit 1; }
cd "$WORKSPACE"

echo "Running pre-commit checks (read-only) in $WORKSPACE"

# --- Bazel hygiene: prevent silent `no-sandbox` creep ---
# `no-sandbox` actions persist their working dir across runs, which
# leaks stale state (see scripts/lint_no_sandbox.py for the doltlite
# WAL incident that motivated this lint). Every new use must be
# explicitly allowlisted with a one-line rationale.
echo "[bazel] no-sandbox allowlist"
python3 scripts/lint_no_sandbox.py

# --- Python (ruff + pyright via uv) ---
echo "[python] ruff check"
uv run ruff check .

echo "[python] ruff format --check"
uv run ruff format --check .

echo "[python] pyright"
uv run pyright

# --- TypeScript (frankweiler/ui) ---
if [ -d frankweiler/ui ]; then
    echo "[ui] vue-tsc"
    # pnpm pinned via frankweiler/ui/package.json's `packageManager`;
    # provisioned on demand via corepack. See scripts/ensure_pnpm.sh.
    # shellcheck source=ensure_pnpm.sh
    source scripts/ensure_pnpm.sh
    (
        cd frankweiler/ui
        if [ ! -d node_modules ]; then
            echo "  installing pnpm deps..."
            pnpm install --frozen-lockfile
        fi
        npx vue-tsc --noEmit
    )
fi

# --- Rust (frankweiler/backend) ---
#
# `cargo fmt --check` is plain cargo (formatter only, no compilation,
# so the doltlite vs system-libsqlite3 link problem doesn't apply).
#
# Clippy used to be skipped here entirely because `cargo clippy`
# couldn't link against our bazel-built doltlite amalgamation. We now
# run clippy through bazel's `rust_clippy_aspect` instead, which
# inherits the same doltlite linkage as a normal `bazelisk build` —
# no cargo-side workaround needed. See .bazelrc's `--config=clippy`
# block for flag wiring.
if [ -d frankweiler/backend ]; then
    echo "[rust] cargo fmt --check"
    (cd frankweiler/backend && cargo fmt --all -- --check)

    # Clippy is run via bazel's `rust_clippy_aspect`. We only invoke
    # it when this script is run interactively
    # (`bazel run :precommit`), not as a `bazel test` fixture.
    #
    # Why: `bazel test :precommit_test` holds the bazel server lock
    # for the duration of the test. If we shelled out to
    # `bazelisk build --config=clippy //...` here, the inner
    # bazelisk would block forever waiting for the same lock — the
    # test would never finish. (Observed: "Testing
    # //:precommit_test; 137s … local" hanging indefinitely.)
    #
    # `bazel run :precommit` releases the lock before exec'ing the
    # script, so the interactive path runs clippy as expected.
    if [[ -n "${BUILD_WORKSPACE_DIRECTORY:-}" ]]; then
        echo "[rust] bazelisk build --config=clippy //..."
        bazelisk build --config=clippy //...
    else
        echo "[rust] skipping bazelisk clippy under \`bazel test\`" \
             "(would deadlock on the bazel server lock — run" \
             "\`bazel run //:precommit\` or" \
             "\`bazel build --config=clippy //...\` directly)"
    fi
fi

echo "All pre-commit checks passed."
