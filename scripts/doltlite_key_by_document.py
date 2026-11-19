#!/usr/bin/env -S uv run python
"""What keying a render store's rows by their document costs and saves on
a store larger than the page cache: the same rows under the key
`grid_rows` has now and under `(markdown_uuid, uuid)`. The dated results
are in `hack/doltlite_key_by_document/README.md`; the write-up is
`docs/dev/doltlite.md` § "What a write costs".

    bazelisk build //third-party/doltlite:doltlite
    uv run python scripts/doltlite_key_by_document.py [scratch_dir] [docs]

The three layouts:

- **by_row**: `PRIMARY KEY (uuid)` and an index on `markdown_uuid`, as
  `grid_rows` is today.
- **by_document**: `PRIMARY KEY (markdown_uuid, uuid)` and a unique
  index on `uuid`, which the render's `ON CONFLICT (uuid)` upsert needs.
- **by_document_no_uuid_index**: the same key, no index, and the upsert
  conflicting on the key: what the render would do if it found a row
  moved between documents some other way.

The rows are modelled on a Slack render store: a row's uuid starts with
its message's time (the v8 layout, `docs/dev/entity_ids.md`), a
document's id is an unstamped hash, documents have 1 to 12 rows (about 4
on average) whose times fall close together, and documents that started
at about the same time interleave. Each timed step runs with the file's
pages evicted from the page cache first:

- **read**: every row of `K` random documents, by document;
- **re-render**: the render's write for those documents, in one
  transaction: `DELETE ... WHERE markdown_uuid = ? AND uuid NOT IN (...)`
  and then each row's upsert `ON CONFLICT (uuid)` with a changed payload,
  then a `dolt_commit`. Reported with how much the file grew.
"""

import ctypes
import hashlib
import mmap
import os
import random
import subprocess
import sys
import tempfile
import time
import uuid

DOLTLITE = os.path.abspath("bazel-bin/third-party/doltlite/doltlite")
PAYLOAD = 700
K = 5_000
BUILD_BATCH = 20_000
REPEATS = 3

DOC_KEYED = (
    "CREATE TABLE g (markdown_uuid TEXT NOT NULL, uuid TEXT NOT NULL, body TEXT, "
    "PRIMARY KEY (markdown_uuid, uuid))"
)

# Each layout's DDL and the conflict target its upsert names.
LAYOUTS = {
    "by_row": (
        [
            "CREATE TABLE g (uuid TEXT PRIMARY KEY, markdown_uuid TEXT NOT NULL, body TEXT)",
            "CREATE INDEX g_by_markdown ON g (markdown_uuid)",
        ],
        "uuid",
    ),
    "by_document": ([DOC_KEYED, "CREATE UNIQUE INDEX g_by_uuid ON g (uuid)"], "uuid"),
    "by_document_no_uuid_index": ([DOC_KEYED], "markdown_uuid, uuid"),
}


def v8(ms, salt):
    h = uuid.uuid5(uuid.NAMESPACE_URL, salt).int & ((1 << 74) - 1)
    return str(uuid.UUID(int=(ms << 80) | (8 << 76) | (2 << 62) | (h & ~(3 << 62))))


def doc_id(i):
    h = hashlib.blake2b(f"doc{i}".encode(), digest_size=16).hexdigest()
    return f"00000000-0000-8{h[:3]}-{h[3:7]}-{h[7:19]}"


def corpus(docs, seed=7):
    """(doc, [row uuids]) per document; rows in time order overall."""
    rnd = random.Random(seed)
    out = []
    t0 = 1_600_000_000_000
    for i in range(docs):
        start = t0 + i * 60_000 + rnd.randrange(600_000)
        n = min(12, 1 + int(rnd.expovariate(1 / 3.3)))
        rows = [v8(start + rnd.randrange(3_600_000), f"{i}/{j}") for j in range(n)]
        out.append((doc_id(i), rows))
    return out


def sql_str(s):
    return "'" + s.replace("'", "''") + "'"


def run(path, sql, readonly=False):
    args = [DOLTLITE] + (["-readonly"] if readonly else []) + [path]
    p = subprocess.run(args, check=False, input=sql, capture_output=True, text=True)
    if p.returncode != 0 or "Error" in p.stderr:
        raise SystemExit(f"doltlite failed on {path}: {p.stderr[:500]}")
    return p.stdout


def evict(path):
    """Drops the file's pages from the page cache, without root: map it
    shared and `msync(MS_INVALIDATE)` the mapping (macOS)."""
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_long,
    ]
    libc.msync.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if addr in (None, ctypes.c_void_p(-1).value):
            raise OSError(ctypes.get_errno(), f"mmap {path}")
        try:
            if libc.msync(addr, size, 2) != 0:  # MS_INVALIDATE
                raise OSError(ctypes.get_errno(), f"msync {path}")
        finally:
            libc.munmap(addr, size)
    finally:
        os.close(fd)


def build(path, ddl, docs):
    rows = [(d, u) for d, us in docs for u in us]
    rows.sort(key=lambda r: r[1])
    run(path, ";\n".join(ddl) + ";\n")
    for i in range(0, len(rows), BUILD_BATCH):
        chunk = rows[i : i + BUILD_BATCH]
        values = ",".join(
            f"({sql_str(u)},{sql_str(d)},hex(randomblob({PAYLOAD // 2})))"
            for d, u in chunk
        )
        run(
            path,
            f"BEGIN; INSERT INTO g (uuid, markdown_uuid, body) VALUES {values}; COMMIT;\n",
        )
    run(path, "SELECT dolt_commit('-Am', 'build'); SELECT dolt_gc();\n")


def timed(path, sql, readonly=False):
    evict(path)
    started = time.monotonic()
    run(path, sql, readonly)
    return time.monotonic() - started


def median(times):
    return sorted(times)[len(times) // 2]


def rerender_sql(picked, target):
    """The render's write for `picked`, its upsert conflicting on `target`."""
    write = ["BEGIN;"]
    for d, us in picked:
        keep = "[" + ",".join(f'"{u}"' for u in us) + "]"
        write.append(
            f"DELETE FROM g WHERE markdown_uuid = {sql_str(d)} "
            f"AND uuid NOT IN (SELECT value FROM json_each('{keep}')) RETURNING uuid;"
        )
        for u in us:
            write.append(
                f"INSERT INTO g (uuid, markdown_uuid, body) VALUES ({sql_str(u)}, {sql_str(d)}, "
                f"hex(randomblob({PAYLOAD // 2}))) ON CONFLICT ({target}) DO UPDATE SET "
                f"body = excluded.body WHERE g.markdown_uuid IS excluded.markdown_uuid;"
            )
    return "\n".join(write)


def main():
    scratch = sys.argv[1] if len(sys.argv) > 1 else tempfile.mkdtemp(prefix="keybydoc-")
    n_docs = int(sys.argv[2]) if len(sys.argv) > 2 else 250_000
    docs = corpus(n_docs)
    n_rows = sum(len(r) for _, r in docs)
    picked = random.Random(11).sample(docs, min(K, n_docs))
    ids = "[" + ",".join(f'"{d}"' for d, _ in picked) + "]"
    read = f"SELECT count(*), sum(length(body)) FROM g WHERE markdown_uuid IN (SELECT value FROM json_each('{ids}'));\n"
    print(f"{n_docs} documents, {n_rows} rows of ~{PAYLOAD} bytes; K = {K} documents")
    paths, steps = {}, {}
    for name, (ddl, target) in LAYOUTS.items():
        body = rerender_sql(picked, target)
        steps[name] = {
            "read": (read, True),
            "re-render, rolled back": (body + "\nROLLBACK;\n", False),
            "committed": (
                body + "\nCOMMIT; SELECT dolt_commit('-Am', 'rerender');\n",
                False,
            ),
        }
        path = paths[name] = os.path.join(scratch, f"{name}.doltlite_db")
        for p in (path, os.path.join(scratch, f".{name}.doltlite_db-lock")):
            if os.path.exists(p):
                os.remove(p)
        started = time.monotonic()
        build(path, ddl, docs)
        print(
            f"{name}: built {os.path.getsize(path) / 1e6:.0f} MB "
            f"in {time.monotonic() - started:.0f} s"
        )
    # Every repeat takes each layout in turn, so whatever else the machine
    # is doing lands on all of them alike, not on whichever ran last.
    runs = {(n, s): [] for n in LAYOUTS for s in ("read", "re-render, rolled back")}
    for _ in range(REPEATS):
        for name in LAYOUTS:
            for step, times in runs.items():
                if step[0] == name:
                    sql, ro = steps[name][step[1]]
                    times.append(timed(paths[name], sql, ro))
    print(f"median of {REPEATS} cold runs, interleaved across layouts; each run listed")
    for name in LAYOUTS:
        print(name)
        for step in ("read", "re-render, rolled back"):
            times = runs[(name, step)]
            print(
                f"  {step:24s} {median(times):6.2f} s   {[round(t, 2) for t in times]}"
            )
    # Committed once: each commit keeps its pages, so a repeat would time a
    # different store.
    for name in LAYOUTS:
        sql, _ = steps[name]["committed"]
        before = os.path.getsize(paths[name])
        secs = timed(paths[name], sql)
        grew = os.path.getsize(paths[name]) - before
        print(
            f"{name}: re-render, committed {secs:6.2f} s   file grew {grew / 1e6:.1f} MB"
        )


if __name__ == "__main__":
    main()
