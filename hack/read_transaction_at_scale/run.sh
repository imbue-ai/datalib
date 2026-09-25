#!/bin/bash
# Step 1 of docs/dev/plans/paged_grids.md, at full size: does a reader
# holding a read transaction as its snapshot cost the writer anything on
# a real-sized store?
#
# Runs the same sealing writer twice against a fresh copy of a grid_index
# store: alone, and beside a read-only connection that holds a transaction
# for HOLD_MS at a time (200 by default), reading throughout. Prints, per run: the writer's
# seal times, what the reader saw, and how much the file grew. A refused
# seal or a failed read stops the run with the error.
#
#   Usage:    ./run.sh [<grid_index db.doltlite_db>]
#   Tunables: SEALS=40 ROWS=1000 HOLD_MS=200 ./run.sh
#             HOLD_MS=600000 holds one transaction across every seal.
#
# The roles are `//datalib/backend/etl:doltlite_two_process`, the helper the
# hermetic scenarios in `etl/tests/doltlite_two_process.rs` drive. The
# copies go under _work/, about the store's size each, one at a time.

set -euo pipefail
cd "$(dirname "$0")"
ROOT="$(git rev-parse --show-toplevel)"

STORE="${1:-$HOME/datalib/stay_alive_1/unified_index/grid_index/db.doltlite_db}"
SEALS="${SEALS:-40}"
ROWS="${ROWS:-1000}"
HOLD_MS="${HOLD_MS:-200}"
WORK="_work"

if ! built=$(cd "$ROOT" && bazelisk build //datalib/backend/etl:doltlite_two_process 2>&1); then
  echo "$built" >&2
  exit 1
fi
HELPER="$ROOT/bazel-bin/datalib/backend/etl/doltlite_two_process"
[ -f "$STORE" ] || { echo "no store at $STORE" >&2; exit 2; }

run() { # <name> <reader role, or "none">
  local name="$1" role="$2" dir="$WORK/$1"
  rm -rf "$dir" && mkdir -p "$dir"
  cp "$STORE" "$dir/db.doltlite_db"
  local size0; size0=$(stat -f %z "$dir/db.doltlite_db")
  local reader=""
  if [ "$role" != none ]; then
    "$HELPER" "$role" --db "$dir/db.doltlite_db" --table grid_rows --hold-ms "$HOLD_MS" \
      --until "$dir/writer.json" --ready-out "$dir/ready" --out "$dir/reader.json" &
    reader=$!
    while [ ! -f "$dir/ready" ]; do
      kill -0 "$reader" 2>/dev/null || { echo "$name: the reader exited before it was ready" >&2; exit 1; }
      sleep 0.1
    done
  fi
  "$HELPER" seal-existing --db "$dir/db.doltlite_db" --table grid_rows --column diff_status \
    --rows "$ROWS" --seals "$SEALS" --out "$dir/writer.json"
  [ -n "$reader" ] && wait "$reader"

  echo "== $name (reader: $role)"
  jq -r --argjson size0 "$size0" '
    (.commits | map(.ms) | sort) as $ms
    | "  writer: \(.commits | length) seals, none refused; seal ms median \($ms[($ms|length)/2|floor]), max \($ms[-1])"
    , "  file grew \((.size_after - $size0) / 1048576 * 10 | round / 10) MB"' "$dir/writer.json"
  if [ "$role" = txn-read ]; then
    jq -r '
      "  read transaction: \(.samples | map(.txn) | unique | length) transactions, \(.samples | group_by(.txn) | map(map(.head) | unique | length) | max) most commits read inside one"' "$dir/reader.json"
  fi
  rm -f "$dir/db.doltlite_db"
}

[ -n "${SKIP_ALONE:-}" ] || run alone none
run read-transaction txn-read
