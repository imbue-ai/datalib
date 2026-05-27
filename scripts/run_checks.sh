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
# We run `cargo fmt --check` only — `cargo clippy` is intentionally
# skipped here. Reason: under Bazel, `libsqlite3-sys` is statically
# linked against the doltlite amalgamation built by
# `//third-party/doltlite:sqlite3` (see MODULE.bazel's
# `crate.annotation` for the wiring). Cargo can't see that annotation,
# so a plain `cargo clippy` either fails outright (no system
# libsqlite3) or links against a system sqlite that lacks symbols
# doltlite ships (sqlite3_load_extension, sqlite3_unlock_notify), and
# spits out misleading errors.
#
# Correctness lints already run inside Bazel: every `rust_library` /
# `rust_binary` rule invokes rustc with `--cap-lints=allow` lifted at
# the crate level by default, and the build itself fails on the
# warnings rustc raises. Anything clippy would catch above and beyond
# that is an inner-loop developer concern, not a CI gate.
#
# To run clippy locally, set up a working libsqlite3 for cargo
# (`brew install sqlite` plus the env vars in
# frankweiler/backend/.cargo/config.toml) and invoke
# `cd frankweiler/backend && cargo clippy --all-targets --all-features`
# by hand.
if [ -d frankweiler/backend ]; then
    echo "[rust] cargo fmt --check"
    (cd frankweiler/backend && cargo fmt --all -- --check)
fi

echo "All pre-commit checks passed."
