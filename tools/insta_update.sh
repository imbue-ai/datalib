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

# shellcheck disable=SC2086
exec "${INSTA_TEST_BIN}" ${INSTA_TEST_ARGS:-} --nocapture
