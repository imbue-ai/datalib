# fsindex storage & doltlite scaling notes

This file records what we learned pushing fsindex toward its design
scale (tens of millions of files) on doltlite, and *how* we learned it,
so the schema decisions in [`schema_raw.rs`](schema_raw.rs) don't look
arbitrary later. Everything here was measured with the stock `doltlite`
CLI (a sqlite3-drop-in) and the `fsindex` binary; the repro snippets are
small enough to re-run.

doltlite version during this work: **v0.11.9 → v0.11.12**.

## TL;DR — the load-bearing facts

1. **A column `DEFAULT` clause makes `dolt_commit` O(n²).** This was the
   single biggest gotcha; it made commits of ~100k+ rows take minutes
   and a million effectively never finish. Fixed by removing the
   `DEFAULT 0` from the bookkeeping schema; filed upstream as
   [dolthub/doltlite#1424](https://github.com/dolthub/doltlite/issues/1424).
2. **The path dominates the on-disk size**, and doltlite does not yet
   compress chunks, so a 60-char path costs ~181 B/row stored (~3×).
3. **The two-table split (`files` vs `file_stats`) is load-bearing for
   cross-tree dedup**, not just for clean `dolt diff`. Keep it.
4. **`dolt_commit` must run before `dolt_gc`** on a connection, and gc
   needs ~2× the db size in free disk.

## 1. The `DEFAULT`-clause O(n²) commit bug

**Symptom.** Rescans of a real `~/` "hung." Tracing showed they were
stuck in `dr::open`'s rescue commit, not the load or the walk. A
`dolt_commit` of a ~100k-row working set took minutes; 1M never
finished.

**How we found it.** Bisected a pure-CLI repro down to a single column:

```sql
-- SLOW: commit ~1.3s at 40k rows, never finishes at 100k (O(n²))
CREATE TABLE t (id TEXT PRIMARY KEY, a INTEGER DEFAULT 0);
-- FAST: identical data, commit ~0.02s at every size
CREATE TABLE t (id TEXT PRIMARY KEY, a INTEGER NOT NULL);
```

`INSERT` and `dolt_gc` stay O(n) and fast; only `dolt_commit` blows up,
and only when *some* column has a `DEFAULT`. `NOT NULL` alone is fine;
even inserting explicit values into the defaulted column is still slow —
it's the schema declaration, not the data.

**Consequence for us.** The framework's `bookkeeping_ddl_for` used
`attempt_count INTEGER NOT NULL DEFAULT 0`. We dropped the `DEFAULT`
(every writer already binds `attempt_count` explicitly, so it was a
no-op) — see the comment in
[`doltlite_raw::bookkeeping_ddl_for`](../../../../src/doltlite_raw.rs).
fsindex then dropped the bookkeeping sidecars entirely (below), so it
sidesteps the bug regardless.

Reported: [dolthub/doltlite#1424](https://github.com/dolthub/doltlite/issues/1424).

## 2. Where the bytes go (and why ~1 GB / 10M is hard today)

Measured gc'd size, 1M rows, ~60-char realistic paths:

| schema | gc'd | per-row |
|--------|------|---------|
| `id(path) + size` only            | 181 MB | **181 B** |
| `+ blake3` as TEXT (64 hex)       | 250 MB | +69 |
| `+ blake3` as BLOB (32 raw)       | 215 MB | +34 |
| `+ index on blake3` (hex)         | 416 MB | **+166** |
| `+ index on blake3` (blob)        | 346 MB | +131 |

Reading this:

- **The path is ~181 B/row** for a 60-char path — a ~3× overhead, because
  doltlite stores the full path per row (no prefix compression yet) plus
  prolly-tree structure.
- **A secondary index re-stores the path** as its row back-reference
  (~130–166 B/row). Indexes are the second-biggest cost after the path.
- **blake3 as 64-char hex wastes ~35 B/row** vs a 32-byte BLOB (×2: in
  the table and its index).

The current fsindex schema (`files` + 1 blake3 index + `file_stats`)
lands at **~557 B/file** on realistic paths → **~5.5 GB / 10M**.

### Decisions taken from this

- **blake3 stored as a 32-byte `BLOB`, not 64-char hex.** Rendered as hex
  only for human output (test snapshots, ad-hoc `hex(blake3)` queries).
  The directory tree-hash also concatenates raw 32-byte child digests.
- **Only `blake3` is indexed.** Dropped `files_by_kind` (3-value,
  low-cardinality, and the one hot query — the rescan cache JOIN — is
  PK-driven anyway) and `files_by_identity_uuid` (almost entirely NULL).
  Both are additive to re-add if a real workload wants them.

Net: 1M synth db **453 MB → 218 MB**; realistic-path projection
~700 → ~557 B/file.

### The ~1 GB / 10M target needs cross-path compression

Paths are *extremely* compressible — but only across the collection:

| | size (1M paths) | B/path |
|---|---|---|
| raw | 71 MB | 71 |
| gzip -9 (whole corpus) | 6 MB | **6** (11×) |
| zstd -19 (whole corpus) | 3 MB | **3** (18×) |
| per-path *independent* gzip | — | **84** (worse than raw!) |

The redundancy is entirely the shared directory prefixes. Capturing it
**per-row independently breaks down** (short strings + per-blob header
overhead), and capturing it **per-collection breaks `dolt diff`** (a
single path's bytes would depend on its neighbors → non-deterministic).

The clean way to capture it while staying deterministic is **per-chunk
compression at the storage layer** — a prolly chunk holds thousands of
rows, compresses ~near the corpus ratio, stays content-addressed
(deterministic), and decompresses transparently so row-level diff is
intact. doltlite doesn't do this yet; it's an open upstream issue:
[dolthub/doltlite#655 "Add per-chunk compression (snappy)"](https://github.com/dolthub/doltlite/issues/655).
**When that lands, our ~5.5 GB / 10M should shrink toward the ~1 GB
target for free, with no schema change.** That's the bet — we're not
doing an app-level path-compression or tree-restructure now.

(SQLite itself has no built-in column compression; `zipvfs`/CEROD are
proprietary, and `sqlite-zstd` is moot because doltlite isn't stock
SQLite.)

## 3. Why the `files` / `file_stats` split is load-bearing

The split (content in `files`, the Unison `(mtime, inode, …)` cursor in
`file_stats`) was originally motivated by clean `dolt diff files`. We
considered **merging** them to save the duplicated path (~32% smaller for
a single root) — and it would have been a mistake.

**Two reasons not to merge:**

a. **You don't need the split for a clean diff.** `dolt_diff_<table>`
   exposes `to_<col>`/`from_<col>` per column, so a content-only diff is
   a projection, not a schema requirement:

   ```sql
   SELECT to_id, from_blake3, to_blake3 FROM dolt_diff_files
   WHERE to_commit = ? AND (from_blake3 IS NOT to_blake3
                            OR from_kind IS NOT to_kind
                            OR from_size IS NOT to_size);
   -- mtime-only changes are filtered out
   ```

b. **But the split is what enables cross-tree dedup** — the real reason
   to keep it. Measured (300k rows, two commits = two "trees", same
   content, *different inodes*, un-gc'd):

   | | tree A | tree B (same content, diff inodes) | B added |
   |---|---|---|---|
   | **split** (`files` content + `file_stats` inode) | 72 MB | 99 MB | **27 MB** |
   | **merged** (inode in the content row) | 47 MB | 94 MB | **47 MB** |

   With the split, tree B's `files` rows are byte-identical → their
   chunks dedup; only `file_stats` (the differing inodes) adds storage.
   Merged, the inode sits in every row, so every row differs and
   *nothing* dedups — tree B re-stores the full content. This is exactly
   the "scan two roots into two branches and share the overlap" property
   (inodes/mtimes are storage-specific, not content), so the split must
   stay.

   Crossover: single-root, merged is smaller (pays the path once);
   multi-root, the split wins as trees accumulate (~3 trees breakeven).

**The remaining inefficiency** is that `file_stats` still carries the
path (its PK), so it's the per-tree cost (~90 B/row, mostly path).
Keying `file_stats` by a *path-derived surrogate int* instead of the
path string would shrink that to ~16 B/row while keeping `files.id =
path` (so content diff identity is preserved). Noted as a future lever;
not done.

## 4. Commit / gc operational rules

- **`dolt_commit` before `dolt_gc`, on the same connection.** The reverse
  (gc then commit) fails with `failed to flush` at scale (reproduced at
  1M rows; fine at 100k). So the fsindex binary does write → `dolt_commit`
  → `dolt_gc`.
- **One sqlite transaction OOMs at multi-million-row scale** (doltlite
  buffers an open transaction's working-set delta in memory). So we
  write in batches (one sqlite tx per `BATCH_SIZE` rows) and seal with a
  single `dolt_commit`. `BATCH_SIZE` is the memory-vs-amplification knob.
- **gc needs ~2× the db size in free disk.** Per-batch transactions
  create chunk "novelty" that only gc reclaims; on a near-full disk a
  large un-gc'd store (tens of GB) can fail to gc (`gc sweep phase
  failed`). gc is therefore **best-effort** in the binary — a failed gc
  warns and leaves a larger db, but the scan + commit still succeed.
  (Per-chunk compression upstream, #655, would also shrink the un-gc'd
  size and make gc easier.)

## How the cursor fast-rescan performs (validated)

With all of the above, the Unison `(mtime, size, inode)` cursor works as
intended at scale: a 1M-file rescan of an unchanged tree **reused all
1,000,000 hashes, rehashed 0**, loaded the cache in ~4 s, and committed a
near-empty diff in ~0.3 s. First scan throughput is I/O-bound on hashing
(~370–500 MB/s of file content); rescans skip hashing entirely.
