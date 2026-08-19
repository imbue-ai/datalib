#!/usr/bin/env bash
# Read-only mirror of scripts/pre-commit, for `bazelisk run //:precommit`.
# Same checks, but never mutates the working tree (no auto-format, no
# `git add`).
#
# This script is NO LONGER a `bazel test`. It used to be exposed as
# `//:precommit_test` so `bazel test //...` would catch lint problems,
# which meant the default test path ran host `uv`/`pnpm`/`npx` against
# the real source tree with the developer's `$HOME` — resolving ruff,
# pyright and vue-tsc from PyPI and npm at test time. Those three now run
# as sandboxed Bazel tests instead (`//:lint`), so this wrapper's only
# job is to be a convenient single entry point that also covers the two
# things Bazel genuinely cannot sandbox:
#
#   * scripts/lint_repo.py — has to enumerate every file in the repo
#   * clippy               — needs the bazel server lock released, which
#                            only the `bazel run` path does
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

# --- Repo hygiene (not sandboxable: reads the whole repo via git) ---
# Checks the `no-sandbox` allowlist and that no first-party Python file
# has escaped the Bazel lint roots. See scripts/lint_repo.py.
echo "[repo] hygiene lints"
python3 scripts/lint_repo.py

# --- Lint + typecheck, via bazel ---
# ruff (python), pyright (python), vue-tsc (typescript). All three are
# hermetic bazel tests now, so this is just a convenience alias — a plain
# `bazelisk test //...` runs exactly the same targets.
echo "[lint] bazelisk test //:lint"
bazelisk test //:lint

# --- Rust (datalib/backend) ---
#
# Formatting is NOT checked here with `cargo fmt`. The `rustfmt_aspect`
# is always-on for every bazel build/test (see .bazelrc's
# `--aspects=...%rustfmt_aspect` + `--output_groups=+rustfmt_checks`),
# so the enclosing `bazel test //...` already fails fast on any
# misformatted crate — a redundant `cargo fmt --check` here would add
# nothing. It would also break CI outright: the devcontainer image
# deliberately ships no host `cargo`/`rustc` (Rust is built entirely
# through rules_rust), so shelling out to `cargo fmt` exited 127
# ("cargo: command not found"). Same reasoning that moved clippy off
# host `cargo` and onto the bazel aspect, below.
#
# Clippy used to be skipped here entirely because `cargo clippy`
# couldn't link against our bazel-built doltlite amalgamation. We now
# run clippy through bazel's `rust_clippy_aspect` instead, which
# inherits the same doltlite linkage as a normal `bazelisk build` —
# no cargo-side workaround needed. See .bazelrc's `--config=clippy`
# block for flag wiring.
if [ -d datalib/backend ]; then
    echo "[rust] bazelisk build --config=clippy //..."
    bazelisk build --config=clippy //...
fi

echo "All pre-commit checks passed."
