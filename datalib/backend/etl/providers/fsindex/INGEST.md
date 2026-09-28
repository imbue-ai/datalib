# fsindex — extract

A directory-tree scanner. Given a local root, walks the tree and
records every visible entry in a doltlite raw store: files and
symlinks as `(path, kind, size, blake3)` rows in `files`, directories
as `(path, size, entries, blake3, optional identity uuid)` rows in
`dirs`.

The row-level schema is in [`src/ingest/schema_raw.rs`](src/ingest/schema_raw.rs);
the contracts every provider honors are in
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).
How `fsindex` relates to the other two tree scanners, `pdf` and `media`:
[`../media/INGEST.md`](../media/INGEST.md) §"Relationship to `fsindex`
and `pdf`". Storage measurements are in
[`src/ingest/STORAGE_NOTES.md`](src/ingest/STORAGE_NOTES.md).

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
bytes inside. Measured on a 500 000-file scan:

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

Every scan starts by emptying `files`, `dirs` and `scan_meta` (and
`scan_meta_bookkeeping`), then walks the tree fresh. Two reasons this
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

The fast-rescan cache is not in the store, so the truncate does not
touch it: the scan loads this host's prior fingerprints for the root —
cursor and digest together — into memory, and `fswalk::decide` compares
each entry against them, skipping the `read(2)` + `blake3` on
unchanged files.

A reset (`datalib-dag --reset <group>/ingest`) empties the store but not this host's
cache, which lives outside it; to force a full rehash, drop the cache
file.

`files` and `dirs` carry no bookkeeping sidecar; `scan_meta` is the one
table with one, recording when the root was last scanned.

## The fast-rescan trick

Cribbed from Unison's `src/fpcache.ml:243` (`dataClearlyUnchanged`).
For each known path, before opening the file:

1. Stat the path. Cheap on macOS/Linux — one syscall, no I/O.
2. Read the cached `(mtime_ns, size, inode, dev, stamp_kind)` and the
   hash that went with them from this host's fingerprint cache.
3. If `stamp_kind = inode` and `(mtime, size, inode, dev)` all match
   the live stat, the cached digest is still valid — no rehash, no
   file read.
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

`stamp_kind = "rescan"` forces a rehash regardless of what the triple
says. Nothing writes it today; a `stamp_kind` the cache does not
recognise reads as `rescan`, so an unknown writer's row is rehashed.

## Stamping policy

`fsindex` is the only provider in the framework that mutates its
upstream. It is opt-in, gated, and logged.

The gates, in order:

1. **Stamping must be switched on for the scan.** A config-driven
   scan stamps only with `stamp = true` on its ingest step (default
   off); the standalone `fsindex` CLI stamps unless given `--no-stamp`.
2. **The cascaded `.fsindex.yaml` must say `stamp_me_with_uuid: true`.**
   Options cascade root → leaf. A child `.fsindex.yaml` with
   `stamp_me_with_uuid: false` cancels stamping for its subtree.
   Default off.
3. **Stamping is per-directory only.** Files don't get
   breadcrumbs; the options for files are xattrs (lossy across `cp`)
   or a parallel shadow tree, and neither is built.
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

The walker skips `.fsindex.yaml` altogether (`walker.rs`), so it is
neither a `files` row nor part of its directory's tree-hash. Otherwise
the act of stamping would change the directory's hash and every
ancestor's up to the root.

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
- **Move detection** between two scans: `datalib-dirtree-diff`
  ([its README](/datalib/backend/dirtree_diff/README.md)) reports a
  moved subtree as one move.

## `scan_meta.id` is the source id

The per-root metadata table (`scan_meta`) keys by the source's id — its
group id, the directory under the data root — *not* by the absolute
path of the scan root. `abs_path` lives in a regular column and may
change between scans without disturbing the key.

## Several roots in one file: branches

The standalone CLI (`datalib-fsindex`, the `fsindex` Bazel target) takes
`--db <file> --branch <name>`, so two roots can be scanned into two
branches of one file and share every identical subtree's chunks. That
is how [`datalib-dirtree-diff`](/datalib/backend/dirtree_diff/README.md)
compares two trees. A config-driven scan has no branch knob: each
`fsindex` group gets its own store under `<group>/ingest/`.

## Inspecting a scan: what changed?

Each scan is one `dolt_commit`, so "what did this scan change?" is a
diff between the last two commits. Ask `dolt_diff_dirs` first: it is a
few percent of the rows and names every directory anything changed
under, with the subtree's size and entry count on the row. Then
`dolt_diff_files` for the file-level detail — a prolly-tree diff only
descends into changed subtrees, so it stays fast even on a
million-entry tree (≈10 s on a 1.7 M-entry index):

```sh
db=<data_root>/<group>/ingest/entities.doltlite_db
doltlite -readonly -box $db \
  "SELECT diff_type, from_id, to_id, to_entries, to_size
     FROM dolt_diff_dirs
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
      AND diff_type != 'unchanged';"
doltlite -readonly -box $db \
  "SELECT diff_type, from_id, to_id, hex(to_blake3) AS to_blake3
     FROM dolt_diff_files
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
      AND diff_type != 'unchanged';"
```

Filter the diff vtabs with `from_ref` / `to_ref` (branch names,
`HEAD`, `HEAD^1`, `HEAD~N`, or commit hashes all work) — **not**
`from_commit` / `to_commit`, even though the result columns are
`from_*` / `to_*`. Related:

- `SELECT * FROM dolt_diff_stat WHERE from_ref = 'HEAD^1' AND to_ref =
  'HEAD';` — added/modified/removed counts for every table that changed.
  The 3-arg form, `dolt_diff_stat('HEAD^1', 'HEAD', 'files')`, answers
  for one named table.
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

The options file (which is also the breadcrumb) is not indexed and not
part of any tree-hash; see §"Stamping policy".

## What `fsindex` does not do

- No render side. Filesystem entries do not project to `GridRow`.
- No CAS, no `blobs.sqlite`. We hash bytes; we don't store
  them.
- No JSONL wire-event tape. There is no upstream wire to mirror; the
  filesystem itself is the human-inspectable tape.
- No retry semantics for transient failures. A `read(2)` either
  succeeds or it's a real error. An unreadable entry is logged
  (`fsindex_entry_error`) and counted in the `fsindex_phase_breakdown`
  event (`stat_errors`, `read_errors`, `non_utf8_paths`); nothing about
  it is written to the store, and the next scan simply tries it again.

## Open follow-ups

- **The row impls are hand-rolled.** Every `BulkUpsertable` impl in
  `schema_raw.rs` is written out by hand; `#[derive(RawTable)]`
  ([`etl/macros/README.md`](/datalib/backend/etl/macros/README.md)) now
  covers payload-less tables, so they could collapse to it.
