# fsindex storage notes

The measurements behind the schema decisions in
[`schema_raw.rs`](schema_raw.rs), taken toward fsindex's design scale
(tens of millions of files). Each number names the doltlite version it
was taken on. What doltlite does in general — write cost, gc, compression,
diffs — is in [`docs/dev/doltlite.md`](/docs/dev/doltlite.md); this file
keeps only what fsindex decided from it.

## The decisions

1. **`blake3` is a 32-byte `BLOB`, not 64-char hex.** It is rendered as
   hex only for people (test snapshots, `hex(blake3)`). The directory
   tree-hash concatenates raw 32-byte child digests too.
2. **No secondary indexes** — only the two path primary keys. The store's
   jobs are durable storage and the diff between scans, and neither uses
   a secondary index. Every analysis query (duplicate clusters by
   `GROUP BY blake3`, fork and move detection, the cross-branch compare)
   is a whole-corpus scan, so the intended workflow streams the table
   into RAM once and indexes it there. Re-adding one is a one-line
   `CREATE INDEX` if a too-big-for-RAM workload ever appears.
3. **The rescan cursor lives outside the store** (§3).
4. **Writes go in batches, sealed by one `dolt_commit`**, and the
   standalone binary then runs `dolt_gc` best-effort: a failed gc warns
   and leaves a larger file, and the scan still succeeds.
   `BATCH_SIZE` (100 000, `walker.rs`) sets how many rows each SQL
   transaction carries.

## 2. Where the bytes go

1M rows, 55-char synthetic paths, after `dolt_gc`, doltlite 0.50.13:

| schema | size | per row |
|--------|------|---------|
| `id(path) + size` only        | 132 MB | **132 B** |
| `+ blake3` as TEXT (64 hex)   | 200 MB | +68 |
| `+ blake3` as BLOB (32 raw)   | 166 MB | +34 |
| `+ index on blake3` (hex)     | 341 MB | **+141** over the hex row |
| `+ index on blake3` (blob)    | 286 MB | +120 over the blob row |

The path dominates, and a secondary index re-stores it as the index
entry's back-reference, which is why decision 2 drops them all. Stock
SQLite takes 133 MB for the first row's table, so doltlite's own
overhead here is small; what would shrink it is compression, which
doltlite does not do yet
([doltlite.md § Disk space](/docs/dev/doltlite.md#disk-space-and-dolt_gc)).

fsindex's own shape (`files` + `dirs`, 71-char synthetic paths) is
~210 B per file at 500k rows on doltlite 0.50.13, so about 2 GB per 10M
files.

Paths compress well only across the collection — 1M paths are 71 MB
raw, 6 MB gzipped as a corpus, and larger than raw if each is gzipped
alone — so the ~1 GB/10M target waits on per-chunk compression upstream
([dolthub/doltlite#655](https://github.com/dolthub/doltlite/issues/655)).
There is no app-level path compression.

## 3. Why the cursor is not in the store

The Unison cursor `(mtime, size, inode, dev)` lives in a host-local
plain-SQLite cache (`datalib_etl_files::fingerprint_cache`); why it is host
state is in [`etl/files/README.md`](../../../../files/README.md) §"The fingerprint
cache is host state, and deliberately not versioned". Two measurements:

| 100k entries, doltlite 0.50.13 | size | per row |
|---|---|---|
| `files` alone | 17.1 MB | 171 B |
| `files` + a `file_stats` cursor table | 32.2 MB | 322 B |

The cursor table repeats every path as its own key, so it was nearly
half the store. And an inode in a versioned row would stop two scans of
identical content from sharing chunks; with the cursor out, a second
tree or branch adds only what is genuinely new.

The cache also carries the digest, so an unchanged tree scans fast into
any branch or a fresh store: 8000 files / 64 MB rescanned onto a new
branch in 0.63 s against 1.6 s cold (doltlite 0.50.3). A 1M-file rescan
of an unchanged tree reused all 1,000,000 hashes; first-scan throughput
is bound by hashing, ~370–500 MB/s of file content.
