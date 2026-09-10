#!/usr/bin/env bash
# The shipped binaries must write doltlite stores, not plain SQLite.
#
# Runs the staged `datalib-*` binaries (`//datalib/backend:bin`, in
# whatever configuration this test is built with) over the two-vCard
# fixture — ingest, render, grid_index — and reads the commit log of
# both stores back through the doltlite shell. A binary linked against
# stock SQLite writes files the shell refuses (`dolt version-control
# features are not available on stock SQLite databases`), and its
# ingest fails the committed-schema check first anyway; either way this
# test goes red. For the static musl release build it is the one check
# that the binaries carry the engine at all — the musl job's static-ness
# assertion cannot see this, and #366 found a musl build that passed it
# while writing SQLite.
#
# Two ways to run it. As the Bazel test, for the build `bazel test`
# makes. Or standalone with `DATALIB_BIN_DIR` (a `:bin` output
# directory) and `DATALIB_VCF_DIR` (the fixture directory) set, which is
# how test.yml's musl job runs it: a `--platforms` build has no shell
# toolchain for `sh_test` to resolve, so the job builds `:bin` under
# `--config=musl-x86_64` and runs this script itself.

set -euo pipefail

if [[ -n "${DATALIB_BIN_DIR:-}" ]]; then
    dag="${DATALIB_BIN_DIR}/datalib-dag"
    shell="${DATALIB_BIN_DIR}/datalib-doltlite"
    vcf="${DATALIB_VCF_DIR:?set DATALIB_VCF_DIR alongside DATALIB_BIN_DIR}/Bridge.vcf"
else
    f=bazel_tools/tools/bash/runfiles/runfiles.bash
    # shellcheck disable=SC1090
    source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
      || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
      || source "$0.runfiles/$f" 2>/dev/null \
      || source "$0.runfiles/_main/$f" 2>/dev/null \
      || { echo >&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
    dag="$(rlocation _main/datalib/backend/bin/datalib-dag)"
    shell="$(rlocation _main/datalib/backend/bin/datalib-doltlite)"
    vcf="$(rlocation _main/datalib/backend/etl/providers/contacts/tests/fixtures/carddav_tng/Bridge.vcf)"
fi
for p in "$dag" "$shell" "$vcf"; do
    [[ -e "$p" ]] || { echo "missing runfile: $p" >&2; exit 1; }
done
fixture_dir="$(cd "$(dirname "$vcf")" && pwd)"

root="${TEST_TMPDIR:-$(mktemp -d)}/root"
mkdir -p "$root"
cat > "$root/config.toml" <<CONFIG
[[groups]]
id = "contacts"
type = "carddav"

[[steps]]
group = "contacts"
function = "ingest"
[steps.params.common]
input_path = "$fixture_dir"

[[steps]]
group = "contacts"
function = "render_markdown"
inputs = ["contacts/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["contacts/render_markdown"]
CONFIG

# datalib-step resolves as a sibling of datalib-dag, which is how :bin
# lays them out and how the installer does.
"$dag" "$root/config.toml" --now 2369-04-15T00:00:00+00:00 2>"$root/dag.stderr" \
    || { echo "datalib-dag failed:" >&2; cat "$root/dag.stderr" >&2; exit 1; }

commits() {  # <store> — prints the commit count, or fails loudly
    local out
    out="$("$shell" -readonly "$1" "SELECT count(*) FROM dolt_log;" 2>&1)" \
        || { echo "$1: not a doltlite store: $out" >&2; return 1; }
    echo "$out"
}

raw="$root/contacts/ingest/entities.doltlite_db"
grid="$root/unified_index/grid_index/db.doltlite_db"
n_raw="$(commits "$raw")"
n_grid="$(commits "$grid")"
rows="$("$shell" -readonly "$grid" "SELECT count(*) FROM grid_rows;")"
echo "raw store commits: $n_raw; grid store commits: $n_grid; grid_rows: $rows"
[[ "$n_raw" -ge 1 ]]  || { echo "raw store has no commits" >&2; exit 1; }
[[ "$n_grid" -ge 1 ]] || { echo "grid store has no commits" >&2; exit 1; }
[[ "$rows" -ge 1 ]]   || { echo "grid_rows is empty" >&2; exit 1; }
