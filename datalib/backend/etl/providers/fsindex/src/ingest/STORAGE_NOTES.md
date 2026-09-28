# fsindex storage & doltlite scaling notes

Measurements behind the schema decisions in [`schema_raw.rs`](schema_raw.rs),
taken pushing fsindex toward its design scale (tens of millions of
files). Everything here was measured with the `doltlite` CLI and the
`fsindex` binary on doltlite v0.11.9–v0.11.12 unless a section says
otherwise; the tree now pins a much later doltlite, so treat the sizes
as orders of magnitude.

## TL;DR — the load-bearing facts

1. **`DEFAULT` clauses are fine.** One made `dolt_commit` O(n²) on
   doltlite before v0.11.13; see §1.
2. **The path dominates the on-disk size**, and doltlite does not yet
   compress chunks, so a 60-char path costs ~181 B/row stored (~3×).
3. **The rescan cursor lives outside this store entirely** (§3), so two
   scans of identical content dedup across trees and branches, and
   `dolt diff` shows only content.
4. **`dolt_commit` must run before `dolt_gc`** on a connection, and gc
   needs ~2× the db size in free disk.
5. **The store carries zero secondary indexes** — only the two path
   primary keys. The store is for durable storage + prolly-tree diff;
   all analysis is a whole-corpus scan done in RAM. An on-disk
   secondary index nearly doubles per-row size and buys nothing here.

## 1. The `DEFAULT`-clause O(n²) commit bug

On doltlite before v0.11.13, any column with a `DEFAULT` clause made
`dolt_commit` quadratic in the working set: ~1.3 s at 40k rows, never
finishing at 100k, where the same table without the `DEFAULT` committed
in ~0.02 s at every size. It was the schema declaration, not the data.
Upstream fixed it in v0.11.13
([dolthub/doltlite#1424](https://github.com/dolthub/doltlite/issues/1424));
the repro, re-run on v0.11.50, commits in ~3–10 ms through 80k rows
with and without the `DEFAULT`.

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

### Decisions taken from this

- **blake3 stored as a 32-byte `BLOB`, not 64-char hex.** Rendered as hex
  only for human output (test snapshots, ad-hoc `hex(blake3)` queries).
  The directory tree-hash also concatenates raw 32-byte child digests.
- **Zero secondary indexes — only the two path primary keys.** We first
  dropped `files_by_kind` (3-value, low-cardinality; the one hot query,
  the rescan cache JOIN, is PK-driven) and `files_by_identity_uuid`
  (almost entirely NULL), then dropped `files_by_blake3` too. The store's
  only jobs are durable content-addressed storage and prolly-tree diff
  between commits/branches — *neither touches a secondary index* (diff
  walks the PK-ordered chunks). Every analysis query — dup clustering
  (`GROUP BY blake3`), fork/move detection, even the cross-branch sync
  diff (`m.blake3 IS NOT l.blake3`) — is a whole-corpus scan, so the
  intended workflow streams the full table into RAM once (it's sized to
  fit) and indexes it there. A secondary index only earns its keep for
  *selective point lookups against the on-disk store without a full
  scan*, which this store never does — and it costs ~131 B/row (it
  re-stores the path as its back-reference), nearly doubling per-row
  size. Re-adding any is a one-line `CREATE INDEX` if a SQL-side,
  too-big-for-RAM workload ever materializes.

The fsindex schema (`files` + `dirs` + `scan_meta`, no secondary
indexes) lands at **~340 B/file** on realistic paths → **~3.4 GB / 10M**.
(Measured before directories had their own table; they are 1–5% of the
rows, so the per-file figure stands.)

Net of blake3 as a BLOB, no bookkeeping sidecars on the entry tables and
no secondary indexes: a 1M-row synthetic db went from **453 MB to
~215 MB**.

### Measured index cost (why we dropped the last one)

The blake3 index alone, at 1M rows: **215 MB → 346 MB** (+131 B/row, blob
back-reference). That's roughly the size of the entire rest of the row —
indexes were the second-biggest space cost after the path itself. With
no on-disk query that needs it, that's pure overhead.

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
**When that lands, ~3.4 GB / 10M should shrink toward the ~1 GB
target with no schema change.** That's the bet; there is no app-level
path compression or tree restructure.

(SQLite itself has no built-in column compression; `zipvfs`/CEROD are
proprietary, and `sqlite-zstd` is moot because doltlite isn't stock
SQLite.)

## 3. Why the cursor is not in this store at all

The Unison cursor `(mtime, size, inode, dev)` lives in a host-local
plain-SQLite cache (`datalib_etl::fingerprint_cache`), not in the
doltlite store. Inodes in a content row stop two scans of identical
content from deduping; measured at 300k rows, two commits with identical
content and different inodes:

| | tree A | tree B (same content, diff inodes) | B added |
|---|---|---|---|
| cursor in a sibling table (`files` content + `file_stats` inode) | 72 MB | 99 MB | **27 MB** |
| cursor in the content row | 47 MB | 94 MB | **47 MB** |

With the cursor out of the store entirely, nothing host-specific reaches
it, so tree B adds nothing beyond genuinely new content. It also saves
the sibling table's own copy of every path — measured at 100k entries,
`files` + `file_stats` was 29.1 MB (291 B/row) against 14.8 MB
(**148 B/row**) for `files` alone: the cursor was 49% of the store.

Why the cursor is host state and not versioned at all is in
[`etl/README.md`](../../../../README.md) §"The fingerprint cache is host
state, and deliberately not versioned". One measurement belongs here:
on 8000 files / 64 MB, a rescan onto a brand-new branch with the cursor
in the store rehashed all 65.5 MB (1.6 s); with the host cache it reuses
everything (0.63 s) — as does a scan into an entirely different
`.doltlite_db` on the same host.

## 4. Commit / gc operational rules

- **`dolt_commit` before `dolt_gc`, on the same connection.** The reverse
  (gc then commit) fails with `failed to flush` at scale (reproduced at
  1M rows; fine at 100k). So the standalone fsindex binary does write →
  `dolt_commit` → `dolt_gc` (`src/bin/fsindex.rs`).
- **One sqlite transaction OOMs at multi-million-row scale** (doltlite
  buffers an open transaction's working-set delta in memory). So we
  write in batches (one sqlite tx per `BATCH_SIZE` rows, 100 000 in
  `walker.rs`) and seal with a single `dolt_commit`. `BATCH_SIZE` is the
  memory-vs-amplification knob.
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
