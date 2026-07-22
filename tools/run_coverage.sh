#!/usr/bin/env bash
# tools/run_coverage.sh — drive `bazelisk coverage` and produce an
# lcov report that includes Rust subprocesses launched as test data
# deps. See //docs/dev/coverage.md for the why.
#
# Usage:
#   tools/run_coverage.sh <test-target> [<test-target> ...] -- <rust-binary> [<rust-binary> ...]
#
# Anything before `--` is passed to `bazelisk coverage` as a test
# target. Anything after `--` is a rust_binary that the tests invoke
# as a subprocess; we run `llvm-cov export` against it to pull the
# real hit counts out of the .profdata bazel collected.
#
# Example:
#   tools/run_coverage.sh \
#     //tests/fixtures:ingested_tng_test \
#     -- \
#     //frankweiler/backend/dag:datalib_dag \
#     //frankweiler/backend/datalib_step:datalib_step \
#     //frankweiler/backend/signal-backup:signal_make_fixture
#
# Output: /tmp/frankweiler_coverage.lcov (override with $LCOV_OUT).
set -euo pipefail

# Split args on the literal `--`.
TARGETS=()
BINARIES=()
seen_separator=0
for a in "$@"; do
    if [[ "$a" == "--" ]]; then
        seen_separator=1
        continue
    fi
    if (( seen_separator )); then
        BINARIES+=("$a")
    else
        TARGETS+=("$a")
    fi
done

if (( ${#TARGETS[@]} == 0 )); then
    echo "error: pass at least one test target before --" >&2
    exit 2
fi
if (( ${#BINARIES[@]} == 0 )); then
    echo "error: pass at least one rust_binary label after --" >&2
    exit 2
fi

# LLVM tools come from Xcode CLT on macOS.
LLVM_PROFDATA="$(xcrun --find llvm-profdata)"
LLVM_COV="$(xcrun --find llvm-cov)"
export LLVM_PROFDATA LLVM_COV

# Default to instrumenting only the backend; override with $INSTRUMENT.
INSTRUMENT="${INSTRUMENT:-^//frankweiler/backend[/:]}"
LCOV_OUT="${LCOV_OUT:-/tmp/frankweiler_coverage.lcov}"

echo "==> bazelisk coverage (filter=$INSTRUMENT)" >&2
bazelisk coverage \
    "${TARGETS[@]}" \
    --instrumentation_filter="$INSTRUMENT" \
    --test_env=LLVM_PROFDATA \
    --test_env=LLVM_COV

# `bazelisk coverage` above has already built instrumented versions of
# all the rust_binaries the test brings in as `data` deps, and the
# bazel-bin/ symlink points at them. We deliberately do NOT run a
# second `bazelisk build <BINARIES>` here — that would rebuild the
# same files un-instrumented (no --collect_code_coverage), the symlink
# would flip to the new artifact, and `llvm-cov export` would then say
# "no coverage data found" against the binary it sees.

# Convert each binary label `//pkg/path:name` → bazel-bin/pkg/path/name.
resolve_bin() {
    local label="$1"
    local pkg="${label#//}"; pkg="${pkg%%:*}"
    local name="${label##*:}"
    echo "bazel-bin/$pkg/$name"
}

# Find per-test profdata files. `bazel coverage` writes one
# `coverage.dat` per test at
#   <bazel-testlogs>/<pkg-path>/<target-name>/coverage.dat
# We only want the ones for the targets *this run* exercised — a plain
# `find` would pick up stale files from earlier coverage runs whose
# instrumentation hashes don't match these binaries, and `llvm-profdata
# merge` would reject the whole batch with "malformed instrumentation
# profile data: function hash is not a valid integer."
TESTLOGS=$(bazelisk info bazel-testlogs)
PROFDATAS=()
for label in "${TARGETS[@]}"; do
    pkg="${label#//}"; pkg="${pkg%%:*}"
    name="${label##*:}"
    dat="$TESTLOGS/$pkg/$name/coverage.dat"
    if [[ ! -f "$dat" ]]; then
        echo "error: missing coverage.dat for $label (expected at $dat)" >&2
        exit 1
    fi
    PROFDATAS+=("$dat")
done

# Merge per-test profdatas into one.
MERGED="$(mktemp -t frankweiler_merged.profdata.XXXXXX)"
trap 'rm -f "$MERGED"' EXIT
echo "==> llvm-profdata merge (${#PROFDATAS[@]} profile(s))" >&2
"$LLVM_PROFDATA" merge -sparse -o "$MERGED" "${PROFDATAS[@]}"

# Export lcov from each binary. The first binary is the primary
# argument; the rest are `--object` add-ons.
BIN_ARGS=("$(resolve_bin "${BINARIES[0]}")")
for b in "${BINARIES[@]:1}"; do
    BIN_ARGS+=("--object" "$(resolve_bin "$b")")
done

echo "==> llvm-cov export → $LCOV_OUT" >&2
"$LLVM_COV" export \
    --format=lcov \
    --instr-profile="$MERGED" \
    "${BIN_ARGS[@]}" \
    > "$LCOV_OUT"

LINES=$(wc -l <"$LCOV_OUT" | tr -d ' ')
echo "==> done. $LCOV_OUT ($LINES lines)" >&2
echo "==> view with: genhtml -o /tmp/cov-html $LCOV_OUT && open /tmp/cov-html/index.html" >&2
