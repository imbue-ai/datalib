#!/usr/bin/env bash
# Wrapper that makes :ingested_tng's pipeline runnable as a sh_test.
# Coverage experiment: see if `bazel coverage` instruments the
# Rust binaries this test invokes as subprocesses.
set -euo pipefail

# Bazel test runfiles layout: TEST_SRCDIR/<workspace>/...
# Under bzlmod the workspace dir is `_main` by default.
cd "${TEST_SRCDIR}/_main"

SCRIPT="$1"; SYNC_BIN="$2"; SIGNAL_BIN="$3"; DATE="$4"
shift 4

WORKSPACE="${TEST_TMPDIR}/sync_workspace"
mkdir -p "$WORKSPACE"

# Mirror the genrule's invocation exactly — we want coverage of the
# same code path the production genrule exercises.
exec python3 "$SCRIPT" "$SYNC_BIN" "$SIGNAL_BIN" "$DATE" "$WORKSPACE" "$@"
