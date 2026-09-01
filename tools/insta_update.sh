#!/bin/bash
# Wrapper used by `insta_update` bazel sh_binary targets so that
# `bazel run //path:foo.update` updates the `*.snap` files in the
# source tree (not the bazel sandbox).
#
# Plain `bazel test` runs each action in a sandbox whose writes don't
# propagate back to the source tree, and existing `data = glob([...
# *.snap])` deps only stage *reads*, not writebacks. The standard
# insta idiom is therefore to invoke updates via `bazel run` and let
# insta resolve `INSTA_WORKSPACE_ROOT` against `$BUILD_WORKSPACE_DIRECTORY`,
# which bazel sets only under `bazel run` and which points at the
# user's actual workspace.
#
# Insta combines `INSTA_WORKSPACE_ROOT` with the crate-relative path
# it derives from the test source location, so we pass the bazel
# workspace root directly. Don't append a subdir — insta does that
# part itself.
#
# Required env (populated by the bazel rule):
#   INSTA_TEST_BIN     absolute path to the compiled test binary
# Optional env:
#   INSTA_TEST_ARGS    extra args (e.g. `--ignored`) passed verbatim
#                      to the test binary. Space-separated.
set -euo pipefail

: "${BUILD_WORKSPACE_DIRECTORY:?must be invoked via 'bazel run' (BUILD_WORKSPACE_DIRECTORY unset)}"
: "${INSTA_TEST_BIN:?INSTA_TEST_BIN not set — wire up via tools/insta.bzl:insta_update}"

export INSTA_UPDATE=always
export INSTA_WORKSPACE_ROOT="${BUILD_WORKSPACE_DIRECTORY}"
echo "[insta-update] INSTA_WORKSPACE_ROOT=${INSTA_WORKSPACE_ROOT}" >&2

# Hand the test our runfiles tree.
#
# `bazel run` of an sh_binary leaves RUNFILES_DIR unset and drops us in
# `<target>.runfiles/<workspace>`. That is fine for a test that reaches
# its data deps through `$(rootpath …)` env vars — most of them — but a
# test that uses the runfiles *library* finds nothing: it is not the
# runfiles owner, so there is no `<binary>.runfiles` beside it either,
# and `Runfiles::create()` fails with `RunfilesDirNotFound`.
#
# The tree it wants is this wrapper's, which is where `extra_data`
# staged the deps. Derive it by cutting `$PWD` at `.runfiles/` rather
# than assuming the workspace directory is named `_main`.
if [[ -z "${RUNFILES_DIR:-}" && "$PWD" == *.runfiles/* ]]; then
    export RUNFILES_DIR="${PWD%%.runfiles/*}.runfiles"
fi

# shellcheck disable=SC2086
exec "${INSTA_TEST_BIN}" ${INSTA_TEST_ARGS:-} --nocapture
