#!/usr/bin/env bash
# Stages a runtime the way release.yml's `runtime` job does — a
# RELATIVE <dest>, from a directory that is not the repo — and then runs
# the job's two follow-up lines against it. v0.35.0's runtime job was
# the first to pass a relative path, and the script's smoke test,
# which runs from inside the qmd package, could not find the node it
# had just staged.

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

script="$(rlocation _main/scripts/stage_runtime.sh)"
[[ -x "$script" ]] || { echo "ERROR: stage_runtime.sh not in runfiles at $script" >&2; exit 1; }
# The runfiles tree lays the four targets out exactly as bazel-bin does.
tree="${script%/scripts/stage_runtime.sh}"
qmd_pin="$(grep -E -m1 '^pub const DEFAULT_QMD_VERSION:' "$tree/datalib/backend/runtime/src/qmd.rs" | sed -E 's/.*"([^"]+)".*/\1/')"
[[ -n "$qmd_pin" ]] || { echo "ERROR: no DEFAULT_QMD_VERSION pin found" >&2; exit 1; }

work="$TEST_TMPDIR/work"
mkdir -p "$work"
cd "$work"

STAGE_RUNTIME_BAZEL_BIN="$tree" "$script" runtime

# The job's smoke lines, verbatim.
runtime/node/bin/node --version
got="$(runtime/node/bin/node runtime/qmd/*/node_modules/@tobilu/qmd/dist/cli/qmd.js --version)"
echo "$got"
case "$got" in
    "qmd $qmd_pin"*) ;;
    *) echo "ERROR: expected 'qmd $qmd_pin …' from the staged tree, got '$got'" >&2; exit 1 ;;
esac
