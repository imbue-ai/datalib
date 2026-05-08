#!/usr/bin/env bash
# Launch the frankweiler dev UI pointed at the checked-in TNG fixtures.
#
# Materializes a one-shot data root in a tmpdir from the
# `//tests/fixtures:ingested_tng` artifact (qmd tree + mirror.sqlite),
# then execs `frankweiler/dev.sh` against it. The tmpdir is removed on
# exit. No host data, no network — useful for eyeballing what the grid
# looks like with Slack threads + reactions.
#
# Invoke via `bazelisk run //frankweiler:dev_tng`.

set -eo pipefail

f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null \
  || source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f 2- -d ' ')" 2>/dev/null \
  || source "$0.runfiles/$f" 2>/dev/null \
  || source "$0.runfiles/_main/$f" 2>/dev/null \
  || { echo>&2 "ERROR: cannot find bazel runfiles bootstrap"; exit 1; }
set -u

DUMP="$(rlocation _main/tests/fixtures/ingested/dump.sql)"
QMD_TAR="$(rlocation _main/tests/fixtures/ingested/qmd.tar)"
DEV_SH="$(rlocation _main/frankweiler/dev.sh)"
[[ -f "$DUMP" ]]    || { echo "ERROR: dump.sql not found at $DUMP" >&2; exit 1; }
[[ -f "$QMD_TAR" ]] || { echo "ERROR: qmd.tar not found at $QMD_TAR" >&2; exit 1; }
[[ -x "$DEV_SH" ]]  || { echo "ERROR: dev.sh not found at $DEV_SH" >&2; exit 1; }

if ! command -v sqlite3 >/dev/null 2>&1; then
  echo "ERROR: sqlite3 not on PATH (needed to materialize mirror.sqlite)" >&2
  exit 1
fi

ROOT="$(mktemp -d -t frankweiler-tng.XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
echo "TNG data root: $ROOT" >&2

# qmd.tar prefixes every entry with `qmd/` (so the tar self-describes its
# contents); strip that one component so the providers land directly
# under $ROOT, which is where the backend expects them.
tar -xf "$QMD_TAR" -C "$ROOT" --strip-components=1
sqlite3 "$ROOT/mirror.sqlite" < "$DUMP"

exec "$DEV_SH" "$ROOT"
