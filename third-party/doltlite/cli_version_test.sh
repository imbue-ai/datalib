#!/usr/bin/env bash
# Asserts the built doltlite CLI reports the version its BUILD file
# pinned — i.e. that @doltlite_autoconf (which supplies shell.c) and
# @doltlite_amalgamation (which supplies the engine) came from the same
# doltlite release.
#
# Nothing in Bazel couples those two http_archives, so a version bump
# that updates one URL and forgets the other still builds and still
# links; it just yields a CLI whose shell and engine disagree. Running
# the real binary is the cheapest check that catches it.
#
# Also serves as a smoke test that the CLI links and executes at all,
# and that its dolt-SQL surface is present — a shell linked against
# stock SQLite instead of doltlite would pass `--version` but fail on
# `dolt_commit`.
set -euo pipefail

cli="$1"
want="$2"

got="$("${cli}" --version)"

if [[ "${got}" != *"${want}"* ]]; then
    echo "doltlite CLI version mismatch." >&2
    echo "  want (from third-party/doltlite/BUILD.bazel): ${want}" >&2
    echo "  got  (from the built binary):                 ${got}" >&2
    echo >&2
    echo "The doltlite_amalgamation and doltlite_autoconf pins in" >&2
    echo "MODULE.bazel have probably drifted apart — they must name" >&2
    echo "the same doltlite release." >&2
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
