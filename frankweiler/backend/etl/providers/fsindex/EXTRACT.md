# fsindex — extract

A directory-tree scanner. Given a local root, walks the tree and
records `(path, kind, size, blake3, optional identity uuid)` for
every visible entry in a doltlite raw store.

This document covers what's load-bearing and provider-specific.
For the framework contracts every provider honors —
schema-first, bulk-upsert chokepoints, commit lifecycle,
bookkeeping sidecars, `--reset-and-redownload` semantics —
see [`docs/dev/data_architecture_ingestion.md`](../../../../docs/dev/data_architecture_ingestion.md).
For the row-level schema, see
[`src/extract/schema_raw.rs`](src/extract/schema_raw.rs).

## Why two entity tables

The user-visible question `fsindex` answers is "what's in this
tree, and what changed since last time?" That breaks into two
sub-questions whose answers update on completely different
cadences:

1. **What is each entry?** — kind, size, blake3, symlink target,
   identity UUID. Updates only when the content actually changes.
2. **How do we know it hasn't changed without rehashing it?** —
   mtime, size, inode, dev. Updates every scan, even on unchanged
   files (the inode is unchanged but the row's `last_attempt_at`
   bookkeeping moves; the mtime might also move on touch-without-edit
   operations Unison treats as no-ops).

If those lived on one entity table, `dolt diff files` would be
noise-dominated — every touched-but-unchanged file would show up
as a row mutation even though the content is identical. So we
split:

- `files` — the **content** entity. PK=path; typed columns
  (`kind`, `size`, `blake3`, `symlink_target`, `identity_uuid`)
  carry the semantic shape of the entry. Diff here is "what
  really changed."
- `file_stats` — the **cursor** entity. PK=path; typed columns
  (`mtime_ns`, `size`, `stamp_kind`, `inode`, `dev`, `ctime_ns`)
  carry Unison's fast-rescan triple plus discriminator. Diff
  here is noisy by design and nobody reads it semantically.

This split is orthogonal to the framework's events-vs-bookkeeping
split (see
[`docs/dev/data_architecture_ingestion.md`](../../../../docs/dev/data_architecture_ingestion.md)
§"Events vs bookkeeping"). Both `files` and `file_stats` are
**entity** tables in the framework's sense, and each gets its own
`<t>_bookkeeping` sidecar via `dr::bookkeeping_ddl_for` for
attempt-tracking (`attempt_count`, `last_attempt_at`, `last_error`).

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

Every scan starts by `DELETE FROM files; DELETE FROM file_stats;
DELETE FROM scan_meta;` (and their `_bookkeeping` sidecars), then
walks the tree fresh. Two reasons this works:

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
**in memory**: the orchestrator loads the prior `file_stats` rows
and the prior `files.blake3` for `kind='file'` rows BEFORE the
truncate, so `stamp::decide` still has cached state to compare
against and the reuse path still skips the `read(2)` + `blake3`
on unchanged files. See `extract::fetch` for the load-then-truncate
ordering.

The framework's `--reset-and-redownload` flag now means "ignore
the cache too" — force a full rehash of every file even if the
(mtime, size, inode) triple would have allowed reuse. Useful for
verifying nothing has silently drifted.

Caveat: the `<t>_bookkeeping` sidecars get truncated along with
the entity tables, so the running `attempt_count` visible at HEAD
resets to 1 on every scan. The per-commit history is NOT lost —
dolt preserves every prior commit's bookkeeping rows, queryable
via `SELECT … AS OF 'HEAD~N'` and the `dolt_diff_<t>_bookkeeping`
virtual table — so "did this row error on the previous scan?"
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
2. Read the cached `(mtime_ns, size, inode, dev, stamp_kind)` from
   `file_stats`.
3. If `stamp_kind = "inode"` and `(mtime, size, inode, dev)` all
   match the live stat, the cached `files.blake3` is still valid —
   no rehash, no row write. `attempt_count` does not bump (we
   didn't attempt anything).
4. If anything mismatched, open the file, rehash, write the new
   `files` row and the new `file_stats` row in the same batch.

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
  FROM files
  WHERE identity_uuid IS NOT NULL
  GROUP BY identity_uuid
  HAVING COUNT(*) > 1;
  ```
- **Move detection** across branches:
  ```sql
  SELECT a.id AS was_at, b.id AS now_at, a.identity_uuid
  FROM main.files a JOIN laptop2.files b USING(identity_uuid)
  WHERE a.id != b.id;
  ```

## `scan_meta.id` is the source name from config

The per-root metadata table (`scan_meta`) keys by the `name:` from
the `sources:` entry in `config.yaml`, *not* by the absolute path
of the scan root. The `name:` is the same per-source stable
identifier used everywhere else in the framework (`.doltlite_db`
filenames, log lines, render cursor paths), it survives moves of
the data root because it's user-supplied, and it sidesteps the
"what do we do if the root moves?" question entirely — `abs_path`
lives in a regular column and is allowed to evolve between scans
without disturbing the PK. If the user renames a source in config,
the orchestrator treats that as a separate source and the old row
stays put until garbage-collected.

## Multi-root via doltlite branches

Two scan roots that want to share storage and benefit from
prolly-tree dedup point at the same `<name>.doltlite_db` and pick
different `target_doltlite_branch` values in their `sources:`
entries:

```yaml
- name: laptop_home
  type: fsindex
  root: /Users/thad
  doltlite_db: fsindex.doltlite_db
  target_doltlite_branch: laptop

- name: nas_backup
  type: fsindex
  root: /Volumes/nas/thad
  doltlite_db: fsindex.doltlite_db
  target_doltlite_branch: nas
```

The §"Single writer per doltlite file" rule still applies — sync's
orchestrator serializes per-source, so the two roots scan one at a
time even though they share the file. Branch-level diff is the
diff/sync primitive:

```sql
ATTACH 'fsindex.doltlite_db' AS db;
SELECT m.id AS path, m.blake3 AS laptop, n.blake3 AS nas
FROM db.laptop.files m FULL OUTER JOIN db.nas.files n USING(id)
WHERE m.blake3 IS NOT n.blake3;
```

`target_doltlite_branch` defaults to `main`, so a single-root
configuration needs nothing extra.

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
  everything. When invoked **inside `frankweiler-sync`**, the
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
  [`docs/dev/data_architecture_ingestion.md`](../../../../docs/dev/data_architecture_ingestion.md)
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
  [§"Deferred work"](../../../../docs/dev/data_architecture_ingestion.md)
  calls out exactly this gap. Tracked in a follow-up issue; when
  it lands, each `BulkUpsertable` impl in this file collapses to
  its struct definition. Tracked at
  [imbue-ai/mixed_up_files#41](https://github.com/imbue-ai/mixed_up_files/issues/41).
