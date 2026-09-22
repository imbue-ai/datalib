#!/bin/bash
# Wrapper used by `live_run` bazel sh_binary targets: runs the live
# tests that live inside a package's ordinary test binary.
#
# They are ordinary `#[test]`s in a module called `live`, so their
# names all begin `live::` and the package's `rust_test` excludes them
# with `args = ["--skip", "live::"]`. Here we do the opposite and pass
# `live::` as the filter, so this runs exactly the ones CI does not.
#
# `bazel run`, not `bazel test`, for two reasons: a test action would
# still apply the target's own `--skip`, and these tests need the
# invoking shell's environment — `latchkey` reads the host keyring,
# and `LATCHKEY_CURL` has to point at the router curl
# (docs/dev/curl_impersonate.md). `bazel run` inherits it; `bazel
# test` would need a `--test_env` per variable.
#
# Required env (populated by the bazel rule):
#   LIVE_TEST_BIN      absolute path to the compiled test binary
#   LIVE_TEST_FILTER   the name filter that selects the live tests
set -euo pipefail

: "${LIVE_TEST_BIN:?LIVE_TEST_BIN not set — wire up via tools/live.bzl:live_run}"

# Hand the test our runfiles tree: `bazel run` of an sh_binary leaves
# RUNFILES_DIR unset and drops us in `<target>.runfiles/<workspace>`,
# where a test that uses the runfiles library finds nothing. Same
# reasoning as tools/insta_update.sh.
if [[ -z "${RUNFILES_DIR:-}" && "$PWD" == *.runfiles/* ]]; then
    export RUNFILES_DIR="${PWD%%.runfiles/*}.runfiles"
fi

echo "[live] ${LIVE_TEST_BIN} ${LIVE_TEST_FILTER} $*" >&2
exec "${LIVE_TEST_BIN}" "${LIVE_TEST_FILTER}" --nocapture "$@"
