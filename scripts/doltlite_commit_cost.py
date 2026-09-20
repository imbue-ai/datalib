#!/usr/bin/env python3
"""What a doltlite write costs on disk, by key shape, transaction shape and
commit cadence — the same rows in every variant. The dated results are in
`hack/doltlite_commit_cost/README.md`; the write-up is
`datalib/backend/etl/README.md` § "What a write costs".

    bazelisk build //third-party/doltlite:doltlite
    python3 scripts/doltlite_commit_cost.py [scratch_dir]

Each variant builds a 100k-row table, reports the file size before and
after `dolt_gc()`, and the last one squashes its commits to show what gc
can reclaim once the intermediate commits are gone.
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import uuid

DOLTLITE = os.path.abspath("bazel-bin/third-party/doltlite/doltlite")
N, BATCH = 100_000, 500


def random_key(i):
    return hashlib.blake2b(str(i).encode(), digest_size=16).hexdigest()


def time_prefixed_key(i):
    """v8 layout: 48-bit unix ms, then bits of a hash — sorts in time order."""
    ms = 1_700_000_000_000 + i // 20
    h = uuid.uuid5(uuid.NAMESPACE_URL, str(i)).int & ((1 << 74) - 1)
    return str(uuid.UUID(int=(ms << 80) | (8 << 76) | (2 << 62) | (h & ~(3 << 62))))


def value(i):
    return hashlib.sha256(str(i).encode()).hexdigest()


def size(d):
    return sum(
        os.path.getsize(os.path.join(d, f)) for f in os.listdir(d) if f.startswith("s.")
    )


def sql(db, text):
    p = subprocess.run(
        [DOLTLITE, db], input=text, capture_output=True, text=True, check=False
    )
    if p.returncode:
        sys.exit(p.stderr)
    return p.stdout.strip()


def variant(scratch, label, keyfn, one_txn, commit_every_batches, squash=False):
    d = os.path.join(scratch, "v")
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    db = os.path.join(d, "s.doltlite_db")
    lines = [
        "CREATE TABLE t(k TEXT PRIMARY KEY, v TEXT);",
        "SELECT dolt_commit('-Am','schema');",
    ]
    if one_txn:
        lines.append("BEGIN;")
    for b in range(N // BATCH):
        rows = ",".join(
            f"('{keyfn(i)}','{value(i)}')" for i in range(b * BATCH, (b + 1) * BATCH)
        )
        lines.append(f"INSERT INTO t VALUES {rows};")
        if commit_every_batches and (b + 1) % commit_every_batches == 0:
            lines.append(f"SELECT dolt_commit('-Am','batch {b}');")
    if one_txn:
        lines.append("COMMIT;")
    if not commit_every_batches or (N // BATCH) % commit_every_batches:
        lines.append("SELECT dolt_commit('-Am','final');")
    sql(db, "\n".join(lines) + "\n")
    before = size(d)
    commits = sql(db, "SELECT count(*) FROM dolt_log;")
    sql(db, "SELECT dolt_gc();")
    after = size(d)
    print(
        f"{label:52s} commits={commits:>4}  before gc {before / 1e6:6.1f} MB  after gc {after / 1e6:6.1f} MB"
    )
    if squash:
        base = sql(db, "SELECT commit_hash FROM dolt_log WHERE message LIKE 'schema%';")
        h0 = sql(db, "SELECT dolt_hashof_table('t');")
        sql(
            db,
            f"SELECT dolt_reset('--soft','{base}'); SELECT dolt_commit('-Am','squashed');",
        )
        assert sql(db, "SELECT dolt_hashof_table('t');") == h0, (
            "squash changed the table"
        )
        sql(db, "SELECT dolt_gc();")
        print(
            f"{'  … squashed to one commit, gc again':52s} commits={sql(db, 'SELECT count(*) FROM dolt_log;'):>4}"
            f"  {'':22s} after gc {size(d) / 1e6:6.1f} MB"
        )


def main():
    if not os.path.exists(DOLTLITE):
        sys.exit(f"{DOLTLITE} missing: bazelisk build //third-party/doltlite:doltlite")
    scratch = (
        sys.argv[1]
        if len(sys.argv) > 1
        else tempfile.mkdtemp(prefix="doltlite_commit_cost_")
    )
    os.makedirs(scratch, exist_ok=True)
    print(
        f"doltlite {sql(os.path.join(scratch, 'probe.doltlite_db'), 'SELECT dolt_version();')}, "
        f"{N} rows, {BATCH} rows per INSERT statement\n"
    )
    variant(scratch, "random keys, 200 txns, commit once", random_key, False, 0)
    variant(scratch, "random keys, ONE txn, commit once", random_key, True, 0)
    variant(
        scratch, "random keys, 200 txns, commit every 10 txns", random_key, False, 10
    )
    variant(
        scratch,
        "random keys, 200 txns, commit every txn",
        random_key,
        False,
        1,
        squash=True,
    )
    variant(
        scratch,
        "time-prefixed keys, 200 txns, commit once",
        time_prefixed_key,
        False,
        0,
    )
    variant(
        scratch,
        "time-prefixed keys, 200 txns, commit every txn",
        time_prefixed_key,
        False,
        1,
    )
    shutil.rmtree(os.path.join(scratch, "v"), ignore_errors=True)


if __name__ == "__main__":
    main()
