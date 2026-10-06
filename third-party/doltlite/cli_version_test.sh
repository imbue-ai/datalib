#!/usr/bin/env bash
# Smoke test: the built doltlite CLI runs, and its dolt-SQL surface is
# real — a shell linked against stock SQLite would print a version but
# fail on `dolt_commit`. The version it prints is compiled in from
# DOLTLITE_VERSION, so this cannot catch the two MODULE.bazel archives
# drifting apart; //tools:version_pins_test does.
set -euo pipefail

cli="$1"
want="$2"

got="$("${cli}" --version)"

if [[ "${got}" != *"${want}"* ]]; then
    echo "doltlite CLI version mismatch." >&2
    echo "  want (from third-party/doltlite/BUILD.bazel): ${want}" >&2
    echo "  got  (from the built binary):                 ${got}" >&2
    exit 1
fi

# The dolt-SQL surface must be real, not stock SQLite.
db="$(mktemp -d)/smoke.doltlite_db"
"${cli}" "${db}" "CREATE TABLE t(id INTEGER PRIMARY KEY);" >/dev/null
"${cli}" "${db}" "SELECT dolt_commit('-Am','smoke');" >/dev/null
commits="$("${cli}" "${db}" "SELECT COUNT(*) FROM dolt_log;")"

if [[ "${commits}" -lt 1 ]]; then
    echo "doltlite CLI linked, but dolt_log is empty after a commit." >&2
    echo "Is the shell linked against stock SQLite instead of doltlite?" >&2
    exit 1
fi

echo "ok: ${got}, dolt_log has ${commits} commit(s)"
