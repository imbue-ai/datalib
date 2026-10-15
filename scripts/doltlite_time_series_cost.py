#!/usr/bin/env python3
"""What it costs to keep time-series points in a doltlite index, and to
bring them up to date after a sync appended a few points to every series.
The dated results are in `hack/doltlite_time_series_cost/README.md`.

    bazelisk build //third-party/doltlite:doltlite
    python3 scripts/doltlite_time_series_cost.py [scratch_dir] [points ...]
    INT_KEYS=1 python3 scripts/doltlite_time_series_cost.py ...   # integer series keys

For each size it loads `SERIES` series of equal length into
`time_series_points` keyed `(time_series_id, t)`, commits and gcs, then
applies one sync's worth of new points (`APPEND` per series) three ways:

- replace every series: delete all of its points and insert them again,
  old and new (what an index does when a source re-renders everything);
- replace one series only (one source re-rendering one thing);
- append only the new points.

Each reports wall time, peak RSS, file growth before gc and what gc
leaves, plus a range read and a downsampled range read of one series.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

DOLTLITE = os.path.abspath("bazel-bin/third-party/doltlite/doltlite")
SERIES = 1_000
APPEND = 10
SIZES = [1_000_000, 5_000_000, 20_000_000]
T0 = 1_700_000_000_000
STEP_MS = 60_000

INT_KEYS = os.environ.get("INT_KEYS") == "1"
KEY_TYPE = "INTEGER" if INT_KEYS else "TEXT"

SCHEMA = f"""
CREATE TABLE time_series_points(
  time_series_id {KEY_TYPE} NOT NULL,
  t INTEGER NOT NULL,
  value REAL,
  text TEXT,
  PRIMARY KEY (time_series_id, t)
);
CREATE TABLE ids(n INTEGER PRIMARY KEY, time_series_id {KEY_TYPE} NOT NULL);
"""


def series_id(n):
    if INT_KEYS:
        return n
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"series-{n}"))


def sql_id(n):
    return str(n) if INT_KEYS else f"'{series_id(n)}'"


def ids_sql():
    rows = ",".join(f"({n},{sql_id(n)})" for n in range(SERIES))
    return f"INSERT INTO ids VALUES {rows};"


def points_sql(first, count, only=None):
    """Points `first .. first+count` of every series (or of series `only`),
    generated inside the engine so the SQL text stays small."""
    where = f"WHERE ids.n = {only}" if only is not None else ""
    return f"""
INSERT INTO time_series_points
WITH RECURSIVE k(i) AS (SELECT {first} UNION ALL SELECT i + 1 FROM k WHERE i < {first + count - 1})
SELECT ids.time_series_id, {T0} + k.i * {STEP_MS}, (k.i % 997) * 0.25 + ids.n, NULL
FROM ids, k {where} ORDER BY 1, 2;
"""


def size(d):
    return sum(
        os.path.getsize(os.path.join(d, f)) for f in os.listdir(d) if f.startswith("s.")
    )


def run(db, text):
    """Run SQL through the CLI; return (stdout, seconds, peak RSS bytes)."""
    start = time.monotonic()
    p = subprocess.run(
        ["/usr/bin/time", "-l", DOLTLITE, db],
        input=text,
        capture_output=True,
        text=True,
        check=False,
    )
    secs = time.monotonic() - start
    if p.returncode:
        sys.exit(p.stderr)
    m = re.search(r"(\d+)\s+maximum resident set size", p.stderr)
    return p.stdout.strip(), secs, int(m.group(1)) if m else 0


def gc(db):
    run(db, "SELECT dolt_gc();\n")


def mb(n):
    return f"{n / 1e6:8.1f} MB"


def base(scratch, points):
    d = os.path.join(scratch, f"base-{points}")
    if os.path.isdir(d):
        return d
    os.makedirs(d)
    db = os.path.join(d, "s.doltlite_db")
    per = points // SERIES
    _, secs, rss = run(
        db,
        SCHEMA
        + ids_sql()
        + "BEGIN;"
        + points_sql(0, per)
        + "COMMIT;SELECT dolt_commit('-Am','load');\n",
    )
    gc(db)
    print(
        f"{points:>11,} points  load {secs:6.1f}s  rss {mb(rss)}  after gc {mb(size(d))}"
    )
    return d


def update(scratch, base_dir, label, sql_text):
    d = os.path.join(scratch, "v")
    shutil.rmtree(d, ignore_errors=True)
    shutil.copytree(base_dir, d)
    db = os.path.join(d, "s.doltlite_db")
    start = size(d)
    _, secs, rss = run(
        db, "BEGIN;" + sql_text + "COMMIT;SELECT dolt_commit('-Am','sync');\n"
    )
    grown = size(d) - start
    gc(db)
    kept = size(d) - start
    print(
        f"  {label:<34} {secs:6.2f}s  rss {mb(rss)}  written {mb(grown)}  kept after gc {mb(kept)}"
    )
    return db


def reads(db, per):
    one = sql_id(SERIES // 2)
    lo, hi = T0, T0 + per * STEP_MS
    bucket = (hi - lo) // 100 + 1
    for label, q in [
        (
            "range read, one series",
            (
                f"SELECT count(*) FROM (SELECT t, value FROM time_series_points "
                f"WHERE time_series_id = {one} AND t BETWEEN {lo} AND {hi});"
            ),
        ),
        (
            "same, downsampled to 100 buckets",
            (
                f"SELECT count(*) FROM (SELECT (t - {lo}) / {bucket} b, min(value), max(value), avg(value) "
                f"FROM time_series_points WHERE time_series_id = {one} AND t BETWEEN {lo} AND {hi} GROUP BY b);"
            ),
        ),
    ]:
        out, secs, _ = run(db, q + "\n")
        print(f"  {label:<34} {secs * 1000:6.0f} ms  ({out} rows)")


def main():
    scratch = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp()
    sizes = [int(a) for a in sys.argv[2:]] or SIZES
    print(
        f"doltlite {DOLTLITE}\nscratch {scratch}\n{SERIES} series, {APPEND} new points each per sync\n"
    )
    for points in sizes:
        per = points // SERIES
        b = base(scratch, points)
        replace_all = "DELETE FROM time_series_points;" + points_sql(0, per + APPEND)
        one = SERIES // 2
        replace_one = (
            f"DELETE FROM time_series_points WHERE time_series_id = {sql_id(one)};"
            + points_sql(0, per + APPEND, only=one)
        )
        update(scratch, b, "replace every series", replace_all)
        update(scratch, b, "replace one series", replace_one)
        db = update(scratch, b, "append new points only", points_sql(per, APPEND))
        reads(db, per)
        print()


if __name__ == "__main__":
    main()
