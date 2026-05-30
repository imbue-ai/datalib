#!/usr/bin/env bash
# Materialize the ST:TNG fixture into a Beeper Texts-shaped data
# directory. Run this once, point `beeper-download` at the result,
# then walk the rendered output:
#
#     ./build_fixture.sh /tmp/tng-beeper
#     beeper-download --beeper-data-dir /tmp/tng-beeper \
#                     --out /tmp/tng-extract.doltlite_db \
#                     --source signal --source googlechat
#     beeper-inspect --db /tmp/tng-extract.doltlite_db
#
# The Rust integration test (`tests/beeper_tng_e2e.rs`) does the
# same thing in a tempdir.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <target_beeper_data_dir>" >&2
    exit 2
fi

target=$1
here=$(cd "$(dirname "$0")" && pwd)

mkdir -p "$target/local-signal"
mkdir -p "$target/media/local.beeper.com"
mkdir -p "$target/media/localhostlocal-signal"

# Clean any prior fixture before re-loading so re-runs are
# byte-stable (a stale .db with old triggers/indexes can cause
# CREATE TABLE IF NOT EXISTS to no-op around updated schemas).
rm -f "$target/index.db" "$target/index.db-wal" "$target/index.db-shm"
rm -f "$target/local-signal/megabridge.db" \
      "$target/local-signal/megabridge.db-wal" \
      "$target/local-signal/megabridge.db-shm"

sqlite3 "$target/index.db" < "$here/index_db.sql"
sqlite3 "$target/local-signal/megabridge.db" < "$here/local_signal_megabridge.sql"

cp "$here/media/local.beeper.com/TNGRPT01" \
   "$target/media/local.beeper.com/TNGRPT01"
cp "$here/media/localhostlocal-signal/TNGART01" \
   "$target/media/localhostlocal-signal/TNGART01"

echo "TNG Beeper fixture materialized at: $target"
