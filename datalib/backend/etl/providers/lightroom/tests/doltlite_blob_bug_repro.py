#!/usr/bin/env python3
"""
doltlite: values >4057 bytes read from an ordinary SQLite file are backed by a
buffer that is reused across rows, so any query holding more than one row's
value at a time sees them all collapse to a single value.

Usage:  python3 doltlite_bug_repro.py /path/to/doltlite
Tested against doltlite v0.11.50 (amalgamation), macOS arm64.
"""
import hashlib, os, sqlite3, subprocess, sys

DL = sys.argv[1]
SRC = "bug_src.db"
NATIVE = "bug_native.doltlite_db"


def sh(db, sql):
    # stdin, not argv: the literals below are large.
    return subprocess.run([DL, db], input=sql, capture_output=True, text=True).stdout.strip()


def value(i, n):
    """Distinct, deterministic n-byte blob."""
    return (hashlib.sha256(str(i).encode()).digest() * (n // 32 + 1))[:n]


def make_plain_sqlite(n, rows=3):
    if os.path.exists(SRC):
        os.remove(SRC)
    con = sqlite3.connect(SRC)                     # stock SQLite, not doltlite
    con.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, b)")
    for i in range(rows):
        con.execute("INSERT INTO t VALUES (?,?)", (i, value(i, n)))
    con.commit()
    con.close()


def make_native(n, rows=3):
    if os.path.exists(NATIVE):
        os.remove(NATIVE)
    vals = ", ".join(f"({i}, X'{value(i, n).hex().upper()}')" for i in range(rows))
    sh(NATIVE, f"CREATE TABLE t (id INTEGER PRIMARY KEY, b); INSERT INTO t VALUES {vals};")


print("Three rows, each holding a DISTINCT 50 KB blob. Expected answer: 3.\n")

make_plain_sqlite(50_000)
make_native(50_000)
print(f"  stock sqlite3 on the plain file .............. "
      f"{sqlite3.connect(SRC).execute('SELECT COUNT(DISTINCT b) FROM t').fetchone()[0]}")
print(f"  doltlite on the SAME plain file .............. {sh(SRC, 'SELECT COUNT(DISTINCT b) FROM t;')}   <-- BUG")
print(f"  doltlite on a native .doltlite_db ............ {sh(NATIVE, 'SELECT COUNT(DISTINCT b) FROM t;')}   (correct)")

print("\nAffected query shapes (plain file, same three 50 KB rows, expected 3):")
for label, q in [
    ("COUNT(DISTINCT b)", "SELECT COUNT(DISTINCT b) FROM t;"),
    ("GROUP BY b", "SELECT COUNT(*) FROM (SELECT b FROM t GROUP BY b);"),
    ("DISTINCT over a materialised subquery", "SELECT COUNT(*) FROM (SELECT DISTINCT hex(b) FROM t);"),
    ("plain row-at-a-time scan (unaffected)", "SELECT COUNT(*) FROM (SELECT hex(b) FROM t);"),
]:
    print(f"  {label:42} {sh(SRC, q)}")

print("\nSize threshold (plain file, COUNT(DISTINCT b) over 3 distinct rows):")
lo, hi = 4_000, 200_000
while lo + 1 < hi:
    mid = (lo + hi) // 2
    make_plain_sqlite(mid)
    if sh(SRC, "SELECT COUNT(DISTINCT b) FROM t;") == "3":
        lo = mid
    else:
        hi = mid
print(f"  correct up to {lo} bytes; collapses from {hi} bytes up")

print("\nData-corrupting consequence: copying such a table into a doltlite table")
print("whose PRIMARY KEY is not a rowid alias (the copy has to buffer rows to")
print("build the key order), expected 3 distinct blobs:")
make_plain_sqlite(50_000)
for label, ddl in [
    ("dest PRIMARY KEY (g)  -- text key ", "g NOT NULL, id INTEGER, b, PRIMARY KEY (g)"),
    ("dest PRIMARY KEY (id) -- rowid    ", "id INTEGER, g NOT NULL, b, PRIMARY KEY (id)"),
    ("dest keyless                      ", "id INTEGER, g NOT NULL, b"),
]:
    dest = "bug_dest.doltlite_db"
    if os.path.exists(dest):
        os.remove(dest)
    sh(dest, f"ATTACH DATABASE '{SRC}' AS s; CREATE TABLE d ({ddl}); "
             f"INSERT INTO d (id, g, b) SELECT id, 'g'||id, b FROM s.t;")
    print(f"  {label} -> {sh(dest, 'SELECT COUNT(DISTINCT b) FROM d;')} distinct blobs stored"
          f"  (persists across dolt_commit)")
