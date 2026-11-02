#!/usr/bin/env bash
# The shipped binaries must write doltlite stores, not plain SQLite.
#
# Runs the staged `datalib-*` binaries (`//datalib/backend:bin_unstamped`
# — `:bin` minus the per-commit `git-hash`, so this test stays cached
# across commits — in whatever configuration it is built with) over the two-vCard
# fixture — ingest, render, grid_index — and reads the raw store's commit
# log back through the doltlite shell, and the grid index's rows (the index
# is plain SQLite, the one store with no log). A binary linked against
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
    dag="$(rlocation _main/datalib/backend/bin_unstamped/datalib-dag)"
    shell="$(rlocation _main/datalib/backend/bin_unstamped/datalib-doltlite)"
    vcf="$(rlocation _main/datalib/backend/etl/providers/contacts/tests/fixtures/carddav_tng/Bridge.vcf)"
fi
for p in "$dag" "$shell" "$vcf"; do
    [[ -e "$p" ]] || { echo "missing runfile: $p" >&2; exit 1; }
done
fixture_dir="$(cd "$(dirname "$vcf")" && pwd)"

root="${TEST_TMPDIR:-$(mktemp -d)}/root"
mkdir -p "$root"
# Not the host's fingerprint cache: this test's paths are gone by the next run.
export DATALIB_CACHE_DIR="$root/fingerprint_cache"
cat > "$root/config.toml" <<CONFIG
[[groups]]
id = "contacts"
type = "contacts"

[[steps]]
group = "contacts"
function = "ingest"
[steps.params.vcf]
path = "$fixture_dir"

[[steps]]
group = "contacts"
function = "render_markdown"
inputs = ["contacts/ingest"]

[[groups]]
id = "unified_document_index"

[[steps]]
group = "unified_document_index"
function = "grid_index"
inputs = ["contacts/render_markdown"]
CONFIG

# The run takes seconds. Past the deadline it is hung, and a job killed
# by its own timeout prints nothing, so this says what was still running
# and what each thread was blocked in before it gives up. Under 60s, so
# it fires before Bazel's `small` timeout does.
deadline="${DATALIB_LINK_TEST_DEADLINE:-45}"

tree() {  # <pid> — the pid and every process below it
    echo "$1"
    local kids
    if [[ -d /proc/self ]]; then
        local s rest state ppid
        kids=""
        for s in /proc/[0-9]*/stat; do
            rest="$(cat "$s" 2>/dev/null)" || continue
            read -r state ppid _ <<<"${rest##*) }"
            [[ "$ppid" == "$1" ]] && kids+="$(basename "$(dirname "$s")") "
        done
    else
        kids="$(pgrep -P "$1" || true)"
    fi
    local k
    for k in $kids; do tree "$k"; done
}

dump_hang() {  # <dag pid>
    echo "datalib-dag still running after ${deadline}s; what it had said:"
    cat "$root/dag.stderr"
    local p t
    for p in $(tree "$1"); do
        if [[ -d "/proc/$p" ]]; then
            echo "--- pid $p: $(tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null)"
            tr '\0' '\n' <"/proc/$p/environ" 2>/dev/null | grep '^DATALIB_DAG_STEP=' || true
            for t in "/proc/$p"/task/*; do
                # `syscall`'s first field is the call it is in (x86_64: 7
                # poll, 72 fcntl, 202 futex, 230 clock_nanosleep).
                echo "  thread $(basename "$t") $(cat "$t/comm" 2>/dev/null):" \
                    "wchan=$(cat "$t/wchan" 2>/dev/null)" \
                    "syscall=$(cut -d' ' -f1 "$t/syscall" 2>/dev/null)"
                sed -n '1,8s/^/    /p' "$t/stack" 2>/dev/null || true
            done
        else
            ps -o pid=,ppid=,etime=,stat=,command= -p "$p" || true
        fi
    done
    local runs="$root/system/runs/runs.sqlite"
    if [[ -e "$runs" ]]; then
        echo "--- step_runs"
        "$shell" -readonly -header "$runs" "SELECT * FROM step_runs;" || true
        echo "--- the last 40 lines in the run store"
        "$shell" -readonly "$runs" \
            "SELECT l.ts_utc, p.process, l.level, l.target, l.msg
               FROM log l LEFT JOIN processes p USING (process_id)
              ORDER BY l.seq DESC LIMIT 40;" || true
    else
        echo "--- no run store at $runs"
    fi
}

# datalib-step resolves as a sibling of datalib-dag, which is how :bin
# lays them out and how the installer does.
echo "running $dag"
"$dag" "$root/config.toml" --now 2369-04-15T00:00:00+00:00 2>"$root/dag.stderr" &
dag_pid=$!
give_up_at=$((SECONDS + deadline))
while kill -0 "$dag_pid" 2>/dev/null; do
    if ((SECONDS >= give_up_at)); then
        dump_hang "$dag_pid" >&2
        kill -KILL $(tree "$dag_pid") 2>/dev/null || true
        exit 1
    fi
    sleep 0.2
done
wait "$dag_pid" \
    || { echo "datalib-dag failed:" >&2; cat "$root/dag.stderr" >&2; exit 1; }

commits() {  # <store> — prints the commit count, or fails loudly
    local out
    out="$("$shell" -readonly "$1" "SELECT count(*) FROM dolt_log;" 2>&1)" \
        || { echo "$1: not a doltlite store: $out" >&2; return 1; }
    echo "$out"
}

raw="$root/contacts/ingest/entities.doltlite_db"
grid="$root/unified_document_index/grid_index/db.sqlite"
n_raw="$(commits "$raw")"
rows="$("$shell" -readonly "$grid" "SELECT count(*) FROM grid_rows;")"
echo "raw store commits: $n_raw; grid_rows: $rows"
[[ "$n_raw" -ge 1 ]] || { echo "raw store has no commits" >&2; exit 1; }
[[ "$rows" -ge 1 ]]  || { echo "grid_rows is empty" >&2; exit 1; }
