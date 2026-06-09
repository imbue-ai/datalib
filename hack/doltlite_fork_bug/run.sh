#!/bin/bash
# Self-contained reproducer for the doltlite fork() bug.
#
# Downloads both amalgamations fresh from their canonical upstream URLs
# (so provenance is self-evident — no risk of repackaged or edited
# sources), verifies sha256, then compiles fork_vs_db.c twice — once
# against stock SQLite 3.51.0, once against doltlite v0.11.5 — and runs
# both with identical parameters. Reports BUSY counts side by side.
#
#   Usage:   ./run.sh
#   Tunable: SECONDS_TO_RUN=5 ./run.sh   (default: 3)
#
# Expected output (numbers will vary):
#
#   ==================== STOCK SQLITE 3.51.0 ====================
#   results: forks=N inserts_ok=M inserts_BUSY=0   inserts_other=0
#
#   ==================== DOLTLITE  v0.11.5  ====================
#   results: forks=N inserts_ok=M inserts_BUSY=>0  inserts_other=0
#
# Same C source, same workload, same fork pattern. Stock SQLite is
# unaffected. Doltlite hits SQLITE_BUSY because its chunk-store lock
# uses BSD flock(), which is inherited by fork()'d children.

set -u
cd "$(dirname "$0")"

SECONDS_TO_RUN="${SECONDS_TO_RUN:-3}"

SQ_URL="https://www.sqlite.org/2025/sqlite-amalgamation-3510000.zip"
DL_URL="https://github.com/dolthub/doltlite/releases/download/v0.11.5/doltlite-amalgamation-0.11.5.zip"

# sha256 of the canonical upstream zips, captured at the time this
# reproducer was authored. If upstream re-rolls a release in place these
# will mismatch — investigate before disabling the check.
SQ_SHA256="1caf7116f2910600d04473ad69d37ec538fa62fa36adccd37b5e0e43647c98be"
DL_SHA256="c9b6f4dbf46b5fa6c2a8a889ed862997f3b48b03ee745051bb6e9f4008ba66b0"

WORK="./_work"
mkdir -p "$WORK"

fetch_and_verify () {
  local url="$1" zip="$2" want_sha="$3"
  if [ ! -f "$zip" ]; then
    echo "==> downloading $url"
    curl -fsSL -o "$zip.tmp" "$url" || { echo "download failed: $url"; exit 1; }
    mv "$zip.tmp" "$zip"
  fi
  local got_sha
  got_sha=$(shasum -a 256 "$zip" | awk '{print $1}')
  if [ "$got_sha" != "$want_sha" ]; then
    echo "sha256 mismatch for $zip"
    echo "  want: $want_sha"
    echo "  got:  $got_sha"
    exit 1
  fi
}

fetch_and_verify "$SQ_URL" "$WORK/sqlite-amalgamation.zip" "$SQ_SHA256"
fetch_and_verify "$DL_URL" "$WORK/doltlite-amalgamation.zip" "$DL_SHA256"

# Unpack (idempotent). Each zip contains a single top-level directory.
SQ_DIR="$WORK/sqlite-amalgamation-3510000"
DL_DIR="$WORK/doltlite-amalgamation-0.11.5"
[ -d "$SQ_DIR" ] || unzip -q -d "$WORK" "$WORK/sqlite-amalgamation.zip"
[ -d "$DL_DIR" ] || unzip -q -d "$WORK" "$WORK/doltlite-amalgamation.zip"

if [ ! -f "$SQ_DIR/sqlite3.c" ] || [ ! -f "$DL_DIR/doltlite.c" ]; then
  echo "unexpected layout after unzip — expected:"
  echo "  $SQ_DIR/sqlite3.c"
  echo "  $DL_DIR/doltlite.c"
  exit 1
fi

# Common compile flags for both builds (the doltlite amalgamation
# self-defines what it needs; we just suppress warnings).
COMMON_WARN=(
  -Wno-unused-but-set-variable -Wno-unused-function
  -Wno-unused-parameter -Wno-unused-variable
  -Wno-implicit-fallthrough -Wno-sign-compare
)

SQ_VER=$(awk '/^#define SQLITE_VERSION / { gsub("\"","",$3); print $3; exit }' "$SQ_DIR/sqlite3.h")
DL_VER="0.11.5"

echo "==> Building against STOCK SQLite $SQ_VER"
cc -O0 -g "${COMMON_WARN[@]}" \
   -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_COLUMN_METADATA \
   -I "$SQ_DIR" \
   fork_vs_db.c "$SQ_DIR/sqlite3.c" \
   -lpthread -ldl -lm -o /tmp/fvd_sqlite 2> /tmp/fvd_sqlite_warn.txt \
  || { echo "stock-sqlite build failed"; cat /tmp/fvd_sqlite_warn.txt; exit 1; }

echo "==> Building against DOLTLITE v$DL_VER"
cc -O0 -g "${COMMON_WARN[@]}" \
   -DDOLTLITE_PROLLY=1 -DSQLITE_CORE -DSQLITE_THREADSAFE=1 \
   -DSQLITE_ENABLE_COLUMN_METADATA \
   -I "$DL_DIR" \
   fork_vs_db.c "$DL_DIR/doltlite.c" \
   -lpthread -ldl -lm -o /tmp/fvd_doltlite 2> /tmp/fvd_doltlite_warn.txt \
  || { echo "doltlite build failed"; cat /tmp/fvd_doltlite_warn.txt; exit 1; }

run () {
  local tag="$1" bin="$2"
  echo
  echo "==================== $tag ===================="
  rm -f /tmp/fvd_test.db
  "$bin" /tmp/fvd_test.db "$SECONDS_TO_RUN"
  rm -f /tmp/fvd_test.db
}

run "STOCK SQLITE $SQ_VER" /tmp/fvd_sqlite
run "DOLTLITE  v$DL_VER " /tmp/fvd_doltlite
