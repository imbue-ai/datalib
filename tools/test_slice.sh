#!/usr/bin/env bash
# Runs one module of a rust_test binary in a process of its own; see
# tools/test_slice.bzl.
set -euo pipefail

: "${TEST_SLICE_BIN:?TEST_SLICE_BIN not set — wire up via tools/test_slice.bzl}"
: "${TEST_SLICE_FILTER:?TEST_SLICE_FILTER not set}"

listed="$("${TEST_SLICE_BIN}" --list "${TEST_SLICE_FILTER}")"
if ! grep -q ': test$' <<<"${listed}"; then
    echo "test_slice: no test in ${TEST_SLICE_BIN} matches '${TEST_SLICE_FILTER}'" >&2
    exit 1
fi
exec "${TEST_SLICE_BIN}" "${TEST_SLICE_FILTER}" "$@"
