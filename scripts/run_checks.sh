#!/usr/bin/env bash
# Read-only mirror of scripts/pre-commit, for `bazelisk run //:precommit`:
# the one command that checks locally as much of what CI checks as a
# laptop can. Never mutates the working tree (no auto-format, no
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

# CI's pull-request flags, on a host whose actions can share CI's cache
# entries: an action's key holds its flags, so a fastbuild gate never hits
# what CI wrote, and CI builds only on x86_64 Linux. Elsewhere, or with no
# BuildBuddy key in `.bazelrc.user`, there is nothing of CI's to reuse and
# fastbuild compiles faster. `--test_env` is in every test's key, which is
# why CI passes it although only the e2e suite reads it.
# `--remote_download_minimal` keeps a hit's outputs remote unless a local
# action needs them. docs/dev/ci.md §"The local gate and CI's cache".
GATE_FLAGS=()  # expanded as ${X[@]+"${X[@]}"}: bash 3.2 calls an empty array unbound under set -u
if [[ "$(uname -s)/$(uname -m)" == "Linux/x86_64" ]] \
    && grep -qs 'x-buildbuddy-api-key=' .bazelrc.user; then
  GATE_FLAGS=(-c opt --config=release --nostamp --config=pr-tests
    --test_env=PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright
    --remote_download_minimal)
  echo "BuildBuddy key on x86_64 Linux: building with CI's pull-request flags"
else
  echo "Building in fastbuild: no BuildBuddy key, or not x86_64 Linux"
fi

# --- Repo hygiene (not sandboxable: reads the whole repo via git) ---
# Checks the `no-sandbox` allowlist and that no first-party Python file
# has escaped the Bazel lint roots. See scripts/lint_repo.py.
echo "[repo] hygiene lints"
# Via runfiles, so Bazel's pinned Python runs it — see the comment on
# `//:lint_repo`. `BUILD_WORKSPACE_DIRECTORY` is already exported by
# `bazel run`, which is how the script finds the real checkout from
# inside the runfiles tree.
LINT_REPO="$(rlocation _main/lint_repo)" || LINT_REPO=""
if [[ -n "$LINT_REPO" && -x "$LINT_REPO" ]]; then
  "$LINT_REPO"
else
  # Direct invocation outside `bazel run` (or a layout without runfiles).
  uv run python scripts/lint_repo.py
fi

# --- Lint + typecheck, via bazel ---
# ruff (python), pyright (python), vue-tsc (typescript). All three are
# hermetic bazel tests now, so this is just a convenience alias — a plain
# `bazelisk test //...` runs exactly the same targets.
echo "[lint] bazelisk test //:lint"
bazelisk test ${GATE_FLAGS[@]+"${GATE_FLAGS[@]}"} //:lint

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
# no cargo-side workaround needed.
#
# No `--config=clippy`: the aspect is always-on now (see .bazelrc), so a
# plain build runs it. This step sweeps the WHOLE graph, and it has to
# be a `build`, not the test run below: both aspects run only on the
# targets named on the command line, and `--build_tests_only` names
# the tests alone — a library those tests link is built but never
# fmt- or clippy-checked (measured: a misformatted worker.rs passed
# the test line and failed this one).
if [ -d datalib/backend ]; then
    echo "[rust] bazelisk build //... (rustfmt + clippy aspects are always-on)"
    bazelisk build ${GATE_FLAGS[@]+"${GATE_FLAGS[@]}"} //...
fi

# --- Every hermetic test ---
# What CI's `bazel test //...` runs, minus the targets that need a host
# (a browser, the network, the keychain). Those tag filters belong on
# this line and nowhere else — see AGENTS.md, "Running tests".
echo "[test] every hermetic test"
bazelisk test ${GATE_FLAGS[@]+"${GATE_FLAGS[@]}"} //... --build_tests_only \
  --test_tag_filters=-no-sandbox,-requires-network,-external,-manual

echo "All pre-commit checks passed."
