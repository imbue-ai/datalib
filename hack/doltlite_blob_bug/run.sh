#!/bin/bash
# Self-contained reproducer for the doltlite large-value aliasing bug.
# Upstream: https://github.com/dolthub/doltlite/issues/2327
#
# Downloads the official doltlite CLI fresh from its canonical release URL
# (so provenance is self-evident) and drives it against a plain SQLite
# database written by the system `sqlite3`. No build step, no repo deps.
#
#   Usage:    ./run.sh                                  (our pinned version)
#   Tunables: DOLTLITE_VERSION=0.11.52 ./run.sh          (the last broken one)
#             DOLTLITE_BIN=/path/to/doltlite ./run.sh    (skip the download)
#
# Every number below should be 3. Anything else is the bug, and the script
# exits non-zero saying so.
#
# The bug: a value larger than ~4 KB (4057 bytes at the usual 4096-byte
# page_size; the cutover moved with page_size) read out of an ordinary
# SQLite file was backed by a buffer doltlite reused across rows. Any
# consumer holding more than one row's value at a time — DISTINCT, GROUP
# BY, a materialised subquery, or an INSERT..SELECT into a table whose
# PRIMARY KEY is not a rowid alias — saw every value collapse onto the
# first row's bytes. Row counts, length() and typeof() all still looked
# right, nothing errored, and in the INSERT..SELECT case the wrong bytes
# were committed. A plain row-at-a-time scan was unaffected, as were
# doltlite's own tables.
#
# Reproduced identically on v0.11.50, v0.11.52 and an older May build, so
# it was long-standing rather than a recent regression. FIXED upstream in
# v0.11.53 by https://github.com/dolthub/doltlite/pull/2329, which is what
# MODULE.bazel pins; run with DOLTLITE_VERSION=0.11.52 to see it fail.

set -uo pipefail
cd "$(dirname "$0")"

# Keep in step with MODULE.bazel's `doltlite_amalgamation` pin.
VERSION="${DOLTLITE_VERSION:-0.11.53}"
WORK="_work"
mkdir -p "$WORK"

FAILURES=0

# Print one measurement, flagging it when it isn't the expected 3.
row() { # $1 = label (pre-padded), $2 = value, $3 = optional suffix
  local mark=""
  if [[ "$2" != "3" ]]; then mark="   <-- BUG"; FAILURES=$((FAILURES + 1)); fi
  printf '  %s %s%s%s\n' "$1" "$2" "${3:-}" "$mark"
}

if [[ -n "${DOLTLITE_BIN:-}" ]]; then
  DL="$DOLTLITE_BIN"
else
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)  ASSET="doltlite-tools-osx-arm64-${VERSION}.zip" ;;
    Darwin-x86_64) ASSET="doltlite-tools-osx-x64-${VERSION}.zip" ;;
    Linux-aarch64) ASSET="doltlite-tools-linux-arm64-${VERSION}.zip" ;;
    Linux-x86_64)  ASSET="doltlite-tools-linux-x64-${VERSION}.zip" ;;
    *) echo "unsupported platform $(uname -s)-$(uname -m); set DOLTLITE_BIN" >&2; exit 1 ;;
  esac
  URL="https://github.com/dolthub/doltlite/releases/download/v${VERSION}/${ASSET}"
  if [[ ! -f "$WORK/$ASSET" ]]; then
    echo "fetching $URL"
    curl -sSLo "$WORK/$ASSET" "$URL" || { echo "download failed" >&2; exit 1; }
  fi
  echo "sha256: $(shasum -a 256 "$WORK/$ASSET" | cut -d' ' -f1)"
  ( cd "$WORK" && unzip -oq "$ASSET" )
  DL="$WORK/${ASSET%.zip}/doltlite"
  chmod +x "$DL"
fi
echo "doltlite: $(echo .version | "$DL" :memory: | sed -n '1p')"
echo

# A plain SQLite file, written by the SYSTEM sqlite3. Three rows, each a
# DISTINCT 50 KB blob; randomblob() makes them distinct and the assertion
# is only ever "there are 3 of them".
make_plain() { # $1 = payload bytes
  rm -f "$WORK/plain.db"
  sqlite3 "$WORK/plain.db" "
    CREATE TABLE t (id INTEGER PRIMARY KEY, b);
    INSERT INTO t VALUES (0, randomblob($1)), (1, randomblob($1)), (2, randomblob($1));"
}

Q="SELECT COUNT(DISTINCT b) FROM t;"
make_plain 50000
rm -f "$WORK/native.doltlite_db"
"$DL" "$WORK/native.doltlite_db" "
  CREATE TABLE t (id INTEGER PRIMARY KEY, b);
  INSERT INTO t VALUES (0, randomblob(50000)), (1, randomblob(50000)), (2, randomblob(50000));" >/dev/null

echo "three distinct 50KB blobs, COUNT(DISTINCT b):"
row 'stock sqlite3, plain file ..............' "$(sqlite3 "$WORK/plain.db" "$Q")"
row 'doltlite, SAME plain file ..............' "$("$DL" "$WORK/plain.db" "$Q")"
row 'doltlite, native .doltlite_db ..........' "$("$DL" "$WORK/native.doltlite_db" "$Q")"

echo
echo "which query shapes were affected (plain file, expect 3):"
row 'COUNT(DISTINCT b) ......................' "$("$DL" "$WORK/plain.db" "SELECT COUNT(DISTINCT b) FROM t;")"
row 'GROUP BY b .............................' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT b FROM t GROUP BY b);")"
row 'DISTINCT over a materialised subquery ..' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT DISTINCT hex(b) FROM t);")"
row 'plain row-at-a-time scan ...............' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT hex(b) FROM t);")" '   (was never affected)'

echo
echo "size threshold (plain file, COUNT(DISTINCT b) over 3 distinct rows)."
echo "Sizes are kept clear of the boundary: the exact cutover moves with"
echo "the source file page_size (4057 bytes at the usual 4096)."
for n in 2000 50000; do
  make_plain "$n"
  row "$(printf '%6s bytes ...........................' "$n")" "$("$DL" "$WORK/plain.db" "$Q")"
done

echo
echo "data-corrupting consequence — copying into a doltlite table, by"
echo "destination key shape (expect 3 distinct blobs stored):"
make_plain 50000
for spec in "PRIMARY KEY (g)   -- text key:g NOT NULL, id INTEGER, b, PRIMARY KEY (g)" \
            "PRIMARY KEY (id)  -- rowid   :id INTEGER, g NOT NULL, b, PRIMARY KEY (id)" \
            "keyless            :id INTEGER, g NOT NULL, b"; do
  label="${spec%%:*}"; ddl="${spec#*:}"
  rm -f "$WORK/dest.doltlite_db"
  "$DL" "$WORK/dest.doltlite_db" "
    ATTACH DATABASE '$WORK/plain.db' AS s;
    CREATE TABLE d ($ddl);
    INSERT INTO d (id, g, b) SELECT id, 'g'||id, b FROM s.t;
    SELECT dolt_commit('-Am','copy');" >/dev/null
  row "$label" "$("$DL" "$WORK/dest.doltlite_db" "SELECT COUNT(DISTINCT b) FROM d;")"
done

echo
if (( FAILURES )); then
  echo "FAIL: $FAILURES measurement(s) came back wrong on doltlite $VERSION."
  exit 1
fi
echo "OK: every measurement is 3 on doltlite $VERSION."
