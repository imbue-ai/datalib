#!/usr/bin/env bash
# tools/run_coverage.sh — drive `bazelisk coverage` and produce an
# lcov report for Rust binaries that tests launch as subprocesses.
# See //docs/dev/coverage.md for the why.
#
# Usage:
#   tools/run_coverage.sh <test-target> [<test-target> ...] -- <rust-binary> [<rust-binary> ...]
#
# Anything before `--` is passed to `bazelisk coverage` as a test
# target; it may be any kind of test (py_test, rust_test, ...).
# Anything after `--` is a rust_binary the tests invoke as a
# subprocess; `llvm-cov export` needs it on disk to map the hit counts
# back to source.
#
# Examples:
#   tools/run_coverage.sh \
#     //tests/fixtures:ingested_tng_test \
#     -- \
#     //datalib/backend/dag:datalib_dag_bin \
#     //datalib/backend/datalib_step:datalib_step \
#     //datalib/backend/signal-backup:signal_make_fixture
#
#   tools/run_coverage.sh \
#     //datalib/backend/etl:doltlite_two_process_test \
#     -- \
#     //datalib/backend/etl:doltlite_two_process
#
# Output: /tmp/datalib_coverage.lcov (override with $LCOV_OUT).
set -euo pipefail

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

# The LLVM tools must be the ones rustc was built with: a .profraw's
# format follows the compiler's LLVM, and Xcode's lags it. They ship in
# the rules_rust toolchain repo. Several toolchain repos can be present
# (cross targets), so take the first whose llvm-profdata runs on this
# host. $LLVM_PROFDATA and $LLVM_COV override.
find_toolchain_llvm() {
    local candidate
    for candidate in "$(bazelisk info output_base)"/external/rules_rust++rust+rust_*_tools/lib/rustlib/*/bin; do
        if "$candidate/llvm-profdata" --version >/dev/null 2>&1; then
            LLVM_PROFDATA="${LLVM_PROFDATA:-$candidate/llvm-profdata}"
            LLVM_COV="${LLVM_COV:-$candidate/llvm-cov}"
            return
        fi
    done
}
if [[ -z "${LLVM_PROFDATA:-}" || -z "${LLVM_COV:-}" ]]; then
    find_toolchain_llvm
fi
if [[ -z "${LLVM_PROFDATA:-}" || -z "${LLVM_COV:-}" ]]; then
    # A fresh output base has not fetched the toolchain yet.
    bazelisk build --show_result=0 @rules_rust//rust/toolchain:current_rust_toolchain
    find_toolchain_llvm
fi
if [[ -z "${LLVM_PROFDATA:-}" || -z "${LLVM_COV:-}" ]]; then
    echo "error: no runnable llvm-profdata in the rules_rust toolchain; set LLVM_PROFDATA and LLVM_COV" >&2
    exit 1
fi
export LLVM_PROFDATA LLVM_COV

# Default to instrumenting only the backend; override with $INSTRUMENT.
INSTRUMENT="${INSTRUMENT:-^//datalib/backend[/:]}"
LCOV_OUT="${LCOV_OUT:-/tmp/datalib_coverage.lcov}"

# Split post-processing makes the test's coverage directory — every
# .profraw any process under the test wrote — an output kept at
# <testlogs>/<pkg>/<name>/_coverage/. Without it that directory lives
# in the sandbox and is gone once bazel's own collection step has
# exported against the *test* binary, which for a rust_test that only
# spawns helpers is uninstrumented: an empty coverage.dat.
#
# The profraws are then outputs a remote cache would take — 1.0 GB for
# the pipeline test, useful to nobody else — so this invocation uploads
# nothing; the disk cache still keeps the instrumented build.
#
# --nocache_test_results because a cached result neither writes this
# run's profraws nor puts the test binary in bazel-bin, and the export
# then fails on the missing binary.
#
# --instrument_test_targets because an async fn's body is compiled into
# the crate that awaits it. A rust_test that drives a library's async
# code runs the body from the test crate, which bazel otherwise leaves
# uninstrumented, and every line after the signature reads 0.
echo "==> bazelisk coverage (filter=$INSTRUMENT)" >&2
bazelisk coverage \
    "${TARGETS[@]}" \
    --instrumentation_filter="$INSTRUMENT" \
    --instrument_test_targets \
    --nocache_test_results \
    --experimental_split_coverage_postprocessing \
    --experimental_fetch_all_coverage_outputs \
    --noremote_upload_local_results \
    --test_env=LLVM_PROFDATA \
    --test_env=LLVM_COV

# `bazelisk coverage` above has already built instrumented versions of
# all the rust_binaries the test brings in as `data` deps, and the
# bazel-bin/ symlink points at them. We deliberately do NOT run a
# second `bazelisk build <BINARIES>` here — that would rebuild the
# same files un-instrumented (no --collect_code_coverage), the symlink
# would flip to the new artifact, and `llvm-cov export` would then say
# "no coverage data found" against the binary it sees.

resolve_bin() {
    local label="$1"
    local pkg="${label#//}"; pkg="${pkg%%:*}"
    local name="${label##*:}"
    echo "bazel-bin/$pkg/$name"
}

# Only this run's targets: each `_coverage/` is an output of the test
# action, so it holds exactly the profraws of the binaries now in
# bazel-bin. A glob over testlogs would pick up other targets' stale
# ones, and `llvm-profdata merge` rejects a batch with mismatched hashes.
TESTLOGS=$(bazelisk info bazel-testlogs)
PROFRAWS=()
for label in "${TARGETS[@]}"; do
    pkg="${label#//}"; pkg="${pkg%%:*}"
    name="${label##*:}"
    dir="$TESTLOGS/$pkg/$name/_coverage"
    found=("$dir"/*.profraw)
    if [[ ! -e "${found[0]}" ]]; then
        echo "error: no .profraw for $label under $dir — did it run an instrumented binary?" >&2
        exit 1
    fi
    PROFRAWS+=("${found[@]}")
done

MERGED="$(mktemp -t datalib_merged.profdata.XXXXXX)"
trap 'rm -f "$MERGED"' EXIT
echo "==> llvm-profdata merge (${#PROFRAWS[@]} profraw(s) from ${#TARGETS[@]} test(s))" >&2
"$LLVM_PROFDATA" merge -sparse -o "$MERGED" "${PROFRAWS[@]}"

# The first binary is the primary argument; the rest are `--object`.
BIN_ARGS=("$(resolve_bin "${BINARIES[0]}")")
for b in "${BINARIES[@]:1}"; do
    BIN_ARGS+=("--object" "$(resolve_bin "$b")")
done

# Third-party sources are dropped here rather than by
# --instrumentation_filter, which cannot reach them: `llvm-cov export`
# reads the coverage-mapping section out of the linked binary, and that
# section names every file compiled into it. Bazel's own baseline
# enumerates the ~350 first-party files the filter selects; this list is
# what the binary adds on top.
#
# Not cosmetic. Before this, 81% of the report was C we do not own —
# doltlite's sqlite3.c amalgamation alone was 5.4 MB of an 8.0 MB file,
# plus oniguruma and ring's vendored crypto — so `genhtml`'s tree view
# opened on third-party code. With it: 3.2 MB, 489 files, all ours.
IGNORE_RE="${IGNORE_RE:-(^/rustc/|^external/|/external/|oniguruma|sqlite3\.c|_bs\.cargo_runfiles/)}"

echo "==> llvm-cov export → $LCOV_OUT" >&2
"$LLVM_COV" export \
    --format=lcov \
    --instr-profile="$MERGED" \
    --ignore-filename-regex="$IGNORE_RE" \
    "${BIN_ARGS[@]}" \
    > "$LCOV_OUT"

LINES=$(wc -l <"$LCOV_OUT" | tr -d ' ')
HIT=$(grep -c '^DA:[0-9]*,[1-9]' "$LCOV_OUT" || true)
echo "==> done. $LCOV_OUT ($LINES lines, $HIT source lines hit)" >&2
if (( HIT == 0 )); then
    echo "warning: no line was hit — are the binaries after -- the ones the tests ran?" >&2
fi
echo "==> view with: genhtml -o /tmp/cov-html $LCOV_OUT && open /tmp/cov-html/index.html" >&2
