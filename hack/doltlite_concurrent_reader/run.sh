#!/bin/bash
# Can a doltlite reader consume a store while a writer is still appending
# commits to it?
#
# This is the load-bearing question behind "the DAG runner is too
# conservative about concurrency". If a consumer step can pin a
# consistent view of a store that a producer step is still writing, the
# runner no longer has to run producers to completion before starting
# their consumers.
#
# Scenarios:
#   A. uncommitted rows  — are a writer's *dirty* rows visible to another
#                          process? (deterministic, sequential)
#   B. naive reader      — plain SELECT while a writer commits, like every
#                          reader we ship today.
#   C. pinned reader     — dolt_at_<t>('<hash>'), doltlite's AS OF.
#   D. re-pinning reader — dolt_diff_<t>('<old>','<new>'): consume only
#                          the delta between two pins. The streaming shape.
#   E. writer health     — does a concurrent reader cost the writer any
#                          SQLITE_BUSY, or leave anything behind?
#   F. pin durability    — is a pin still readable from a fresh process,
#                          and does dolt_gc() collect it out from under a
#                          slow reader?
#
#   Usage:   ./run.sh
#   Tunable: CHUNKS=6 ./run.sh   (default: 6 commits from the writer)

set -u
cd "$(dirname "$0")"
REPO_ROOT=$(cd ../.. && pwd)

CHUNKS="${CHUNKS:-6}"
ROWS_PER_CHUNK=10

DL="$REPO_ROOT/bazel-bin/third-party/doltlite/doltlite"
if [ ! -x "$DL" ]; then
  echo "==> building the doltlite CLI"
  (cd "$REPO_ROOT" && bazelisk build //third-party/doltlite:doltlite) || exit 1
fi
echo "engine: $("$DL" --version 2>&1)"

WORK="./_work"
rm -rf "$WORK"; mkdir -p "$WORK"
DB="$WORK/stream.doltlite_db"

# One connection per process (a REPL fed by a slow pipe), so the
# per-connection HEAD stays coherent — the same discipline the ETL pool
# enforces with max_connections=1.
writer () {
  {
    echo "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);"
    echo "INSERT INTO t VALUES(0,'seed');"
    echo "SELECT dolt_commit('-Am','seed');"
    sleep 0.6
    for i in $(seq 1 "$CHUNKS"); do
      base=$(( i * 1000 ))
      for r in $(seq 1 "$ROWS_PER_CHUNK"); do
        echo "INSERT INTO t VALUES($(( base + r )),'chunk$i');"
      done
      sleep 0.35          # rows are now dirty, NOT yet committed
      echo "SELECT dolt_commit('-Am','chunk$i');"
      sleep 0.25
    done
  } | "$DL" "$DB" 2>&1
}

naive_reader () {
  { for _ in $(seq 1 10); do echo "SELECT 'sample', count(*) FROM t;"; sleep 0.3; done; } \
    | "$DL" "$DB" 2>&1
}

# Pinning in doltlite 0.50.x: dolt_at_<table>('<commit-ish>') is a
# table-valued function accepting HEAD / HEAD~N / a raw hash. It is a
# pure read — no branch, no checkout, no write to the file.
pinned_reader () {
  local pin
  pin=$("$DL" "$DB" "SELECT commit_hash FROM dolt_log LIMIT 1;" 2>&1)
  echo "  reader pinned to $pin" >&2
  { for _ in $(seq 1 10); do
      echo "SELECT 'sample', count(*) FROM dolt_at_t('$pin');"; sleep 0.3
    done; } | "$DL" "$DB" 2>&1
}

verdict () {
  local tag="$1" file="$2" expect="$3"
  echo
  echo "==================== $tag ===================="
  echo "  counts seen over time: $(grep '^sample|' "$file" | cut -d'|' -f2 | tr '\n' ' ')"
  local distinct
  distinct=$(grep '^sample|' "$file" | cut -d'|' -f2 | sort -u | wc -l | tr -d ' ')
  if [ "$distinct" = "1" ]; then
    echo "  => STABLE: one consistent view throughout   (expected: $expect)"
  else
    echo "  => MOVED: view shifted under the reader, $distinct distinct values   (expected: $expect)"
  fi
}

busy_count () { grep -ci 'database is locked\|busy\|error' "$1" 2>/dev/null || true; }

# ------------------------------------------------------------------- A
echo
echo "### A. Are a writer's UNCOMMITTED rows visible to another process?"
rm -f "$DB"
"$DL" "$DB" "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
             INSERT INTO t VALUES(1,'a'),(2,'b');
             SELECT dolt_commit('-Am','only commit');" > /dev/null 2>&1
"$DL" "$DB" "INSERT INTO t VALUES(3,'dirty'),(4,'dirty');" > /dev/null 2>&1   # no commit
echo "  plain SELECT sees:        $("$DL" "$DB" 'SELECT count(*) FROM t;' 2>&1)"
echo "  dolt_at_t('HEAD') sees:   $("$DL" "$DB" "SELECT count(*) FROM dolt_at_t('HEAD');" 2>&1)"
echo "  => a plain SELECT reads the WORKING SET; dolt_at_<t> reads COMMITTED state"

# ------------------------------------------------------------------- B
echo
echo "### B. Naive reader vs. a committing writer"
rm -f "$DB"; writer > "$WORK/writerB.log" 2>&1 & WPID=$!
sleep 0.9; naive_reader > "$WORK/readerB.log" 2>&1; wait $WPID
verdict "B. NAIVE READER (plain SELECT)" "$WORK/readerB.log" "MOVED"

# ------------------------------------------------------------------- C
echo
echo "### C. Pinned reader vs. a committing writer"
rm -f "$DB"; writer > "$WORK/writerC.log" 2>&1 & WPID=$!
sleep 0.9; pinned_reader > "$WORK/readerC.log" 2>&1; wait $WPID
verdict "C. PINNED READER (dolt_at_t)" "$WORK/readerC.log" "STABLE"

# ------------------------------------------------------------------- D
echo
echo "==================== D. RE-PINNING READER (the streaming shape) ===================="
old_pin=$("$DL" "$DB" "SELECT commit_hash FROM dolt_log WHERE message='chunk2';" 2>&1)
new_pin=$("$DL" "$DB" "SELECT commit_hash FROM dolt_log WHERE message='chunk4';" 2>&1)
echo "  old pin (chunk2): $old_pin"
echo "  new pin (chunk4): $new_pin"
delta=$("$DL" "$DB" "SELECT count(*) FROM dolt_diff_t('$old_pin','$new_pin');" 2>&1)
full=$("$DL" "$DB" "SELECT count(*) FROM dolt_at_t('$new_pin');" 2>&1)
echo "  rows to process incrementally, dolt_diff_t(old,new): $delta"
echo "  rows if it re-read the whole table at the new pin:   $full"

# ------------------------------------------------------------------- E
echo
echo "==================== E. WRITER HEALTH ===================="
for tag in B C; do
  echo "  scenario $tag: busy/locked/error lines in writer log = $(busy_count "$WORK/writer$tag.log")"
done
echo "  final committed rows on main: $("$DL" "$DB" "SELECT count(*) FROM t;" 2>&1)"
echo "  branches left behind by readers: $("$DL" "$DB" "SELECT group_concat(name) FROM dolt_branches;" 2>&1)"

# ------------------------------------------------------------------- F
echo
echo "==================== F. PIN DURABILITY ===================="
GCDB="$WORK/gc.doltlite_db"
"$DL" "$GCDB" "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);
               INSERT INTO t VALUES(1,'a');
               SELECT dolt_commit('-Am','c1');" > /dev/null 2>&1
OLD=$("$DL" "$GCDB" "SELECT commit_hash FROM dolt_log WHERE message='c1';" 2>&1)
for i in 2 3 4 5; do
  "$DL" "$GCDB" "INSERT INTO t VALUES($i,'v$i'); SELECT dolt_commit('-Am','c$i');" > /dev/null 2>&1
done
echo "  old pin: $OLD"
echo "  read from a FRESH process, no held connection: $("$DL" "$GCDB" "SELECT count(*) FROM dolt_at_t('$OLD');" 2>&1) row(s)"
echo "  running dolt_gc(): $("$DL" "$GCDB" "SELECT dolt_gc();" 2>&1)"
echo "  same pin re-read AFTER gc:                    $("$DL" "$GCDB" "SELECT count(*) FROM dolt_at_t('$OLD');" 2>&1) row(s)"
NEW=$("$DL" "$GCDB" "SELECT commit_hash FROM dolt_log LIMIT 1;" 2>&1)
echo "  diff across the gc boundary still works:      $("$DL" "$GCDB" "SELECT count(*) FROM dolt_diff_t('$OLD','$NEW');" 2>&1) changed row(s)"
echo "  => a pin is just a hash: portable across processes, and gc-safe"

echo
echo "(logs under $WORK/)"
