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

if ! command -v dolt >/dev/null 2>&1; then
  echo "ERROR: dolt not on PATH (needed to materialize the Dolt repo)" >&2
  exit 1
fi

ROOT="$(mktemp -d -t frankweiler-tng.XXXXXX)"
trap 'rm -rf "$ROOT"' EXIT INT TERM
echo "TNG data root: $ROOT" >&2

# qmd.tar prefixes every entry with `qmd/` (so the tar self-describes its
# contents); strip that one component so the providers land directly
# under $ROOT, which is where the backend expects them.
tar -xf "$QMD_TAR" -C "$ROOT" --strip-components=1

# Materialize the Dolt repo at <root>/dolt_repo and load the fixture dump
# into it. `dolt init` requires an identity; we pass throwaway values
# since no commits originate from this script. Reusing the genrule's
# byte-stable `dump.sql` keeps the in-Dolt state identical to what the
# Python ingest pipeline would have produced.
mkdir -p "$ROOT/dolt_repo"
(
  cd "$ROOT/dolt_repo"
  dolt init --name "Frankweiler TNG" --email "tng@frankweiler.local" >/dev/null
  { echo "USE dolt_repo;"; cat "$DUMP"; } | dolt sql
)

# Ephemeral Dolt port so two dev_tng instances (or a dev_tng plus the dev
# backend pointed at the user's real root) can coexist on 3306 without
# colliding. Vite still serves on 5173; the backend binds the default
# 8731 unless $FRANKWEILER_BIND is set.
DOLT_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
cat > "$ROOT/config.yaml" <<EOF
data_root: $ROOT
dolt:
  port: $DOLT_PORT
EOF
export FRANKWEILER_CONFIG="$ROOT/config.yaml"

exec "$DEV_SH" "$ROOT"
