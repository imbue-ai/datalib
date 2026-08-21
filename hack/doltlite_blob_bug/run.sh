#!/bin/bash
# Self-contained reproducer for the doltlite large-value aliasing bug.
# Upstream: https://github.com/dolthub/doltlite/issues/2327
#
# Downloads the official doltlite CLI fresh from its canonical release URL
# (so provenance is self-evident) and drives it against a plain SQLite
# database written by the system `sqlite3`. No build step, no repo deps.
#
#   Usage:    ./run.sh
#   Tunables: DOLTLITE_VERSION=0.11.52 ./run.sh
#             DOLTLITE_BIN=/path/to/doltlite ./run.sh   (skip the download)
#
# The bug: a value larger than ~4 KB (4057 bytes at the usual 4096-byte
# page_size; the cutover moves with page_size) read out of an ordinary
# SQLite file is backed by a buffer doltlite reuses across rows. Any consumer
# holding more than one row's value at a time — DISTINCT, GROUP BY, a
# materialised subquery, or an INSERT..SELECT into a table whose PRIMARY
# KEY is not a rowid alias — sees every value collapse onto the first
# row's bytes. Row counts, length() and typeof() all still look right,
# nothing errors, and in the INSERT..SELECT case the wrong bytes are
# committed. A plain row-at-a-time scan is unaffected, and doltlite's own
# tables are unaffected.
#
# Reproduces identically on v0.11.50, v0.11.52 (latest at time of
# writing) and an older May build, so it is long-standing.

set -uo pipefail
cd "$(dirname "$0")"

VERSION="${DOLTLITE_VERSION:-0.11.52}"
WORK="_work"
mkdir -p "$WORK"

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
printf '  stock sqlite3, plain file .............. %s\n' "$(sqlite3 "$WORK/plain.db" "$Q")"
printf '  doltlite, SAME plain file .............. %s   <-- BUG\n' "$("$DL" "$WORK/plain.db" "$Q")"
printf '  doltlite, native .doltlite_db .......... %s\n' "$("$DL" "$WORK/native.doltlite_db" "$Q")"

echo
echo "which query shapes are affected (plain file, expect 3):"
printf '  COUNT(DISTINCT b) ...................... %s\n' "$("$DL" "$WORK/plain.db" "SELECT COUNT(DISTINCT b) FROM t;")"
printf '  GROUP BY b ............................. %s\n' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT b FROM t GROUP BY b);")"
printf '  DISTINCT over a materialised subquery .. %s\n' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT DISTINCT hex(b) FROM t);")"
printf '  plain row-at-a-time scan ............... %s   (unaffected)\n' "$("$DL" "$WORK/plain.db" "SELECT COUNT(*) FROM (SELECT hex(b) FROM t);")"

echo
echo "size threshold (plain file, COUNT(DISTINCT b) over 3 distinct rows)."
echo "Sizes are kept clear of the boundary: the exact cutover moves with"
echo "the source file page_size (4057 bytes at the usual 4096)."
for n in 2000 50000; do
  make_plain "$n"
  printf '  %6s bytes ............................ %s\n' "$n" "$("$DL" "$WORK/plain.db" "$Q")"
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
  printf '  %s %s\n' "$label" "$("$DL" "$WORK/dest.doltlite_db" "SELECT COUNT(DISTINCT b) FROM d;")"
done
