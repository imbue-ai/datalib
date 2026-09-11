# fsindex — extract

A directory-tree scanner. Given a local root, walks the tree and
records every visible entry in a doltlite raw store: files and
symlinks as `(path, kind, size, blake3)` rows in `files`, directories
as `(path, size, entries, blake3, optional identity uuid)` rows in
`dirs`.

This document covers what's load-bearing and provider-specific.
For the framework contracts every provider honors —
schema-first, bulk-upsert chokepoints, commit lifecycle,
bookkeeping sidecars, `--reset-and-redownload` semantics —
see [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).
For the row-level schema, see
[`src/extract/schema_raw.rs`](src/ingest/schema_raw.rs).

## Why two entity tables: `files` and `dirs`

Files and directories are different things, and the difference is
in the columns: a symlink has a `symlink_target` and a directory
never does; a directory carries a rolled-up `size`, an `entries`
count and an `identity_uuid` breadcrumb, none of which a file has.
Two tables let each row carry only what its kind of entry has.

The reason that matters for more than tidiness is the diff. A
directory's `blake3` is a **tree-hash** over its immediate children's
`(name, kind, blake3)` (see [`hash.rs`](src/ingest/hash.rs)), so it
covers the whole subtree and survives a move intact. Both tables are
keyed by root-relative path, so renaming a directory rewrites the key
of every descendant — a 50 000-file subtree move is 100 000 rows in
`dolt_diff_files`. In `dolt_diff_dirs` it is one row per directory
under it, and those rows already tell the whole story: `top3` gone,
`renamed3` arrived with the same digest, 50 010 entries and 438 900
bytes inside. Measured on a 500 000-file scan (2026-09-11):

| | rows | wall |
|---|---|---|
| `dolt_diff_dirs` | 23 | 0.00 s |
| `dolt_diff_files` | 100 000 | 0.22 s to walk, 0.38 s to materialize |

There is no cheaper way to get that summary out of a single table.
Doltlite pushes **no** predicate into `dolt_diff_<t>` or
`dolt_at_<t>` — a `WHERE kind = 'dir'`, a primary-key range, even a
primary-key equality all cost the same full walk as no filter at all
(same measurement: `dolt_at_files … WHERE id = '<one path>'` takes
0.18 s against 0.09 s for an unfiltered `COUNT(*)`). A secondary index
on `kind` would not help the diff either, and would re-store the full
path per row (`STORAGE_NOTES.md` §2). A separate table is the one
arrangement in which "just the directories" is a small walk.

A consumer that wants the subtree story therefore diffs `dirs` first
and reaches into `files` only for paths the directory rows do not
already explain. `datalib-dirtree-diff` does exactly that
([its README](/datalib/backend/dirtree_diff/README.md)).

The two tables stay consistent by construction rather than by
foreign key: the walker emits a directory's row only after every
child's, in the same batch stream, and a directory's tree-hash is
computed from the very child rows it just emitted. `identity_uuid`
is set on `dirs` rows alone, by the post-write stamping pass.

The **rescan cursor** — `(mtime_ns, size, inode, dev, stamp_kind)`
per path — is deliberately in neither table. It is host state
(inodes mean nothing on another machine), so it lives in this
machine's `datalib_etl::fingerprint_cache`, a plain-SQLite file
outside version control; `STORAGE_NOTES.md` §3 has the measurements
behind that.

### Typed columns, no JSONB payload

`fsindex` deviates from every other provider in carrying no
`payload` column on any of its tables. The wire-fidelity argument
that motivates JSONB everywhere else (preserve opaque upstream
bytes verbatim) does not apply: the "wire" here is the OS `stat`
call, whose schema is fixed and trivial, and *we* control the
encoding. At fsindex's design scale (tens of millions of rows) the
JSONB envelope adds ~20 B/row of key/quote overhead plus a
`jsonb_extract` per virtual-column read plus a JSON encode per
write — about a gigabyte of bloat at 50M rows, on top of measurable
CPU. Typed columns directly are smaller, faster, and equally
expressive for this schema. Schema additions become
`ALTER TABLE ADD COLUMN`, which is fine for a schema this small
and stable.

### Truncate-and-rebuild on every scan

Every scan starts by `DELETE FROM files; DELETE FROM dirs;
DELETE FROM scan_meta;`, then walks the tree fresh. Two reasons this
works:

1. **Deletions fall out naturally.** A file present at scan-A and
   gone at scan-B simply doesn't get re-inserted, so it disappears
   from the table. No separate reconciliation pass
   ("DELETE FROM files WHERE id NOT IN (this scan's ids)") to
   maintain and forget to call.
2. **Doltlite's prolly-tree dedup makes the rewrite nearly free.**
   Rows with identical `(id, kind, size, blake3, …)` align on the
   same prolly-tree leaves across commits. The diff between two
   commits is exactly "what changed semantically" — re-inserting
   the same row for an unchanged file is a no-op at the storage
   layer.

The Unison-style fast-rescan cache survives the truncate by living
**in memory**: the orchestrator loads this host's prior fingerprints
— cursor and digest together — BEFORE the truncate, so
`fswalk::decide` still has cached state to compare against and the
reuse path still skips the `read(2)` + `blake3` on unchanged files.
See `ingest::fetch` for the load-then-truncate ordering.

The framework's `--reset-and-redownload` flag now means "ignore
the cache too" — force a full rehash of every file even if the
(mtime, size, inode) triple would have allowed reuse. Useful for
verifying nothing has silently drifted.

Caveat: the `<t>_bookkeeping` sidecars get truncated along with
the entity tables, so the running `attempt_count` visible at HEAD
resets to 1 on every scan. The per-commit history is NOT lost —
dolt preserves every prior commit's bookkeeping rows, queryable
via `dolt_at_<t>_bookkeeping('HEAD~N')` and the
`dolt_diff_<t>_bookkeeping` virtual table — so "did this row error on
the previous scan?"
is still answerable, just not via a single SELECT against HEAD.
What's gone is the running-total semantic ("this row has failed
across 5 sync runs" as a single column value). For fsindex this
is acceptable because the upstream is the local filesystem —
there's no API quota to protect or transient-failure budget to
track across scans. A future provider where the running total
matters would need a different reconciliation strategy.

## The fast-rescan trick

Cribbed from Unison's `src/fpcache.ml:243` (`dataClearlyUnchanged`).
For each known path, before opening the file:

1. Stat the path. Cheap on macOS/Linux — one syscall, no I/O.
2. Read the cached `(mtime_ns, size, inode, dev, stamp_kind)` and the
   hash that went with them from this host's fingerprint cache.
3. If `stamp_kind = inode` and `(mtime, size, inode, dev)` all match
   the live stat, the cached digest is still valid — no rehash, no
   file read. `attempt_count` does not bump (we didn't attempt
   anything).
4. If anything mismatched, open the file, rehash, and write the new
   `files` row and the new cache entry.

The cache holds the **hash as well as the stat**, which is what makes
it work at all: a cursor that only said "unchanged" would still have to
go to the scan store for the digest, and on a fresh branch there would
be nothing there. Holding both means an unchanged tree scans fast into
*any* branch, or into a database that has never seen it — measured at
8000 files / 64 MB, 0.63s against the 1.6s a cold scan costs.

`stamp_kind = "nostamp"` (some FUSE mounts, some network filesystems)
drops the inode check and falls back to `(mtime, size)`. Less safe,
but Unison's own behavior on those filesystems.

`stamp_kind = "rescan"` is the sentinel for "previous run was
interrupted mid-fingerprint of this path; force a rehash regardless
of what the triple says." We set it before opening the file and
clear it on successful hash write.

## Stamping policy

`fsindex` is the only provider in the framework that mutates its
upstream. It is opt-in, gated, and logged.

The gates, in order:

1. **Standalone CLI `--no-stamp` overrides everything to off.**
   Escape hatch for read-only scans where the user does not want
   the filesystem touched.
2. **The cascaded `.fsindex.yaml` must say `stamp_me_with_uuid: true`.**
   Options cascade root → leaf. A child `.fsindex.yaml` with
   `stamp_me_with_uuid: false` cancels stamping for its subtree.
   Default off.
3. **Stamping is per-directory only.** Files don't get
   breadcrumbs. (The honest options for files are xattrs (lossy
   across `cp`) or a parallel shadow tree (complex). Neither is
   built yet.)
4. **A directory is stamped at most once.** If `.fsindex.yaml`
   already carries an `identity:` block, it is not rewritten.
   Removing the `identity:` block manually is the explicit way to
   re-stamp.

When all gates pass, the scanner generates a UUIDv7 (time-ordered),
writes it into `.fsindex.yaml` via atomic rename, and logs at
`info!`:

```
fsindex_stamped path=… uuid=…
```

The breadcrumb file format:

```yaml
# inherited options (user-edited)
ignore:
  - "*.tmp"
  - "node_modules/"
stamp_me_with_uuid: true

# machine-managed; do not hand-edit unless you mean to fork identity.
identity:
  uuid: 0190f8d7-c8aa-7c3e-b4a1-2e2e9b1f0001
  stamped_at: 2026-06-14T11:03:22-07:00
  stamper_version: 1
  originally_at: "Documents/Photos/2019"
```

The breadcrumb file is **excluded from the directory's blake3
tree-hash** — see `schema_raw.rs` §"Directory tree-hash
canonicalization." Otherwise the act of stamping would invalidate
the dir's hash and fan a rehash storm up to the root.

### The UUID is not unique

A `cp -r` of a stamped directory copies the breadcrumb too, so two
directories end up claiming the same identity UUID. That is a real
and expected case, **not a bug to suppress**. The UUID is therefore
not a primary key anywhere — it's an indexed secondary identity
hint, surfaced by these queries:

- **Fork detection** within a scan:
  ```sql
  SELECT identity_uuid, COUNT(*), GROUP_CONCAT(id)
  FROM dirs
  WHERE identity_uuid IS NOT NULL
  GROUP BY identity_uuid
  HAVING COUNT(*) > 1;
  ```
- **Move detection** across branches:
  ```sql
  SELECT a.id AS was_at, b.id AS now_at, a.identity_uuid
  FROM main.dirs a JOIN laptop2.dirs b USING(identity_uuid)
  WHERE a.id != b.id;
  ```

## `scan_meta.id` is the source name from config

The per-root metadata table (`scan_meta`) keys by the source name —
the `<name>` prefix of the step's declared outputs in `config.toml`
(`fsindex-home/ingest` → `fsindex-home`) — *not* by the absolute path
of the scan root. That name is the same per-source stable
identifier used everywhere else in the framework (`.doltlite_db`
filenames, log lines, render cursor paths), it survives moves of
the data root because it's user-supplied, and it sidesteps the
"what do we do if the root moves?" question entirely — `abs_path`
lives in a regular column and is allowed to evolve between scans
without disturbing the PK. If the user renames a source in config,
the runner treats that as a separate source and the old row
stays put until garbage-collected.

## Multi-root via doltlite branches

> **Stale:** the `doltlite_db` / `target_doltlite_branch` knobs this
> section describes are not in the current `FsindexConfig` schema
> (`common` + `stamp` are the only fields, and the config structs are
> `deny_unknown_fields`), so the config below would be rejected. The
> storage design is recorded here because the branch-level diff
> primitive still holds; the config surface for it does not exist yet.

Two scan roots that want to share storage and benefit from
prolly-tree dedup would point at the same `<name>.doltlite_db` and
pick different `target_doltlite_branch` values:

```toml
# NOT CURRENTLY SUPPORTED — see the note above.
[[groups]]
id = "laptop_home"
type = "fsindex"

[[steps]]
group = "laptop_home"
function = "ingest"
[steps.params.fswalk]
path = "/Users/thad"

[[groups]]
id = "nas_backup"
type = "fsindex"

[[steps]]
group = "nas_backup"
function = "ingest"
[steps.params.fswalk]
path = "/Volumes/nas/thad"
```

Today each of those two steps gets its own raw store under
`<group>/ingest/` instead, which is the supported way to scan two roots.

The §"Single writer per doltlite file" rule still applies — the
runner serializes per-source, so two roots sharing a file would scan
one at a time. Branch-level diff is the
diff/sync primitive:

```sql
ATTACH 'fsindex.doltlite_db' AS db;
SELECT m.id AS path, m.blake3 AS laptop, n.blake3 AS nas
FROM db.laptop.files m FULL OUTER JOIN db.nas.files n USING(id)
WHERE m.blake3 IS NOT n.blake3;
```

`target_doltlite_branch` would default to `main`, so a single-root
configuration needs nothing extra.

## Inspecting a scan: what changed?

Each scan is one `dolt_commit`, so "what did this scan change?" is a
diff between the last two commits. Ask `dolt_diff_dirs` first: it is a
few percent of the rows and names every directory anything changed
under, with the subtree's size and entry count on the row. Then
`dolt_diff_files` for the file-level detail — a prolly-tree diff only
descends into changed subtrees, so it stays fast even on a
million-entry tree (≈10 s on a 1.7 M-entry index):

```sh
doltlite -readonly -box <name>.doltlite_db \
  "SELECT diff_type, from_id, to_id, to_entries, to_size
     FROM dolt_diff_dirs
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
      AND diff_type != 'unchanged';"
doltlite -readonly -box <name>.doltlite_db \
  "SELECT diff_type, from_id, to_id, hex(to_blake3) AS to_blake3
     FROM dolt_diff_files
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
      AND diff_type != 'unchanged';"
```

Filter the diff vtabs with `from_ref` / `to_ref` (branch names,
`HEAD`, `HEAD^1`, `HEAD~N`, or commit hashes all work) — **not**
`from_commit` / `to_commit`, even though the result columns are
`from_*` / `to_*`. Related:

- `SELECT * FROM dolt_diff_stat('HEAD^1', 'HEAD', 'files');` — per-table
  added/modified/removed counts (call it with 3 args; the vtab form
  rejects `WHERE`).
- `SELECT * FROM dolt_log();` — the commit history (one row per scan).

See [`docs/dev/doltlite.md`](/docs/dev/doltlite.md) for the full set
of history/diff system tables.

## Options file

Per-directory `.fsindex.yaml`. Options cascade root → leaf; a child
file overrides the inherited value for its subtree. The file is
gitignore-friendly to commit (it's how the data owner expresses
"these ignore rules travel with this tree") but the indexer does
not require it to be committed.

Recognized keys:

| Key                    | Type                | Default | Meaning                                                                                |
|------------------------|---------------------|---------|----------------------------------------------------------------------------------------|
| `ignore`               | `list[str]`         | `[]`    | Gitignore-style patterns. Matched via the `ignore` crate; cascades and accumulates.    |
| `stamp_me_with_uuid`   | `bool`              | `false` | Opt-in to identity-UUID stamping for this directory and its descendants.               |
| `identity`             | `map`               | absent  | Machine-managed breadcrumb. See §"Stamping policy." Hand-edit at your own risk.        |

The options file itself, and the breadcrumb (same file), are
**excluded from the directory's blake3 tree-hash** so they don't
fan rehash storms.

## What's NOT here yet

This document and `schema_raw.rs` are the schema-first deliverable.
The walker, stamp-comparator, hasher, db helper, options parser,
and the standalone `fsindex` binary land in follow-up commits.
The shape they'll take, briefly, so the schema reads as a
contract not a tease:

- **Walker** — `jwalk` for parallel directory traversal + the
  `ignore` crate for cascaded gitignore-shaped matching. Both
  well-trodden Rust.
- **Hasher** — blake3 with mmap above a size threshold; rayon to
  fan out across CPUs. Directory tree-hash via the canonical
  encoding in `schema_raw.rs`.
- **DB** — `bulk_upsert_in_tx` + `bulk_upsert_bookkeeping` per
  §"Bulk-upsert as the standard write path." For the **standalone
  binary**, the binary is its own orchestrator and is allowed to
  commit periodically (proposal: every 100k upserts, configurable)
  so a mid-scan ^C on a tens-of-millions-of-rows tree doesn't lose
  everything. When invoked **inside `datalib-sync`**, the
  one-commit-per-source rule applies as normal.
- **Options** — `.fsindex.yaml` cascade, gitignore patterns via
  the `ignore` crate, atomic breadcrumb write-via-rename.

## What `fsindex` does not do

- No translate side. Filesystem entries don't currently project to
  `GridRow`. A future "filesystem entry" `GridRow` family is the
  natural home if/when we want them in the UI's union view.
- No CAS, no `.blobs.doltlite_db`. We hash bytes; we don't store
  them.
- No JSONL wire-event tape. There is no upstream wire to mirror —
  file-imported sources skip the chokepoint by design (see
  [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
  §"Bulk-upsert as the standard write path"). The filesystem
  itself is the human-inspectable tape.
- No retry semantics for transient failures. A `read(2)` either
  succeeds or it's a real error; we don't have an upstream API
  with 5xx behavior to reason about. Unreadable entries get
  `attempt_count` and `last_error` in the `_bookkeeping` sidecar
  per the framework's universal pattern, and a future scan picks
  them up if they become readable.

## Open follow-ups

- **`#[derive(BulkUpsertable)]` for non-payload tables.** Every
  row impl in this provider's `schema_raw.rs` is hand-rolled
  because the existing `#[derive(WirePayloadRow)]` macro is
  specifically for the JSONB-payload shape and doesn't fit our
  typed-column tables. The doc's
  [§"Deferred work"](/docs/dev/data_architecture_ingestion.md)
  calls out exactly this gap. Tracked in a follow-up issue; when
  it lands, each `BulkUpsertable` impl in this file collapses to
  its struct definition. Tracked at
  [imbue-ai/datalib#41](https://github.com/imbue-ai/datalib/issues/41).

- **Rescan-cache load is sqlx-bound, not engine-bound.** On a
  1.7 M-entry index the in-memory cache load takes ~29 s, but the
  engine scans the same rows in ~6 s (measured with
  `SELECT COUNT(*), SUM(LENGTH(id)), … FROM files`). The ~4.5× gap is
  Rust-side per-row marshalling: sqlx
  allocates a `SqliteRow` and runs `try_get` type-dispatch per
  column (~10 M calls), plus a `String`/`Vec`/struct allocation and
  a `HashMap` insert per row. Cheap win: pre-size the maps from
  `COUNT(*)`. The real win is a lower-level read path — a raw
  doltlite C-API column scan like [`docs/dev/doltlite.md`](/docs/dev/doltlite.md)
  §"`sqlite3_open_v2`" already uses for open — bypassing sqlx's
  per-row overhead. Even bigger: don't full-slurp the cache every
  run (drive the rescan from `dolt_diff` against the prior commit).
