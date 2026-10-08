# `datalib_etl_files` — what changed on disk, for a file-backed source

A source that reads local files asks this crate what changed rather than
walking a folder or re-reading an input itself: `fswalk.rs` walks a tree,
`fingerprint_cache.rs` remembers each file's hash per host, `fsscan.rs`
answers "what is there now and what changed", `file_checkpoint.rs` keeps
what one feed already ingested, and `export_files.rs` finds the files of
an unpacked export (Facebook, LinkedIn). It sits on `datalib_etl`, whose
README has the store rules; a source that never reads a local file does
not link it, so an edit here rebuilds only the sources that do.

## The fingerprint cache is host state, and deliberately not versioned

Every tree-scanning provider keeps a Unison-style cursor so a rescan can skip
hashing a file whose `(mtime, size, inode, dev)` has not moved. It lives in a
host-local cache rather than in the provider's versioned store:

- **It is host state.** An inode number means nothing on another machine, so
  a branch fetched from elsewhere carries a cursor that cannot match — and
  nothing records which host a cursor came from, so you cannot detect it.
- **Branching it is a category error.** The cursor describes the live
  filesystem, which has no history; rolling a branch back does not un-modify
  the files on disk.
- **It was half the store.** Measured at 100k entries on doltlite 0.50.13,
  `files` + `file_stats` in one store is 322 B/row against 171 B/row for
  `files` alone,
  because the cursor re-stores the full path as its own primary key
  (`providers/fsindex/src/ingest/STORAGE_NOTES.md` has the table).

It is plain SQLite (via the `doltlite_engine=sqlite` URI parameter, the same
door `datalib_runs::store` uses), because losing a cache costs a rehash
rather than correctness, and it needs no commits, no history and no prolly
tree.

Keys are **absolute paths**, so one chain per host rather than per root. This
is the part Unison gets wrong: its `fpcache` is per replica *pair*, so
syncing one tree against two peers hashes the same bytes twice, and scanning
a directory tells you nothing about its parent.

It lives at `$DATALIB_CACHE_DIR/fingerprints.sqlite`, else under
`$XDG_CACHE_HOME/datalib`, else `~/Library/Caches/datalib` (macOS) or
`~/.cache/datalib`. **A test names its own `DATALIB_CACHE_DIR`**, and under
a bazel test (`TEST_TMPDIR` set, which the processes a test spawns inherit)
`default_cache_path` refuses to fall through to the host's. A test scans a
sandbox path that is gone by the next run, so in the host cache every one
stays as a dead row: one mac measured 191k of them, 94% of its cache. The
fixture pipeline (`tests/fixtures/run_sync_pipeline.py`) keeps its cache
inside the data root it builds.

## Answering "did it change?" for a file-backed source

`fsscan` walks a tree and hashes only what the host cache cannot vouch for;
`file_checkpoint` stores what one feed already ingested. The split is the
point:

- the **fingerprint cache** is host-wide, shared and unversioned — expensive
  to compute, identical for every consumer, and a description of a machine
  rather than of a history;
- the **cursor** is what *this* source already ingested, and lives in that
  source's own store.

The cache cannot answer "since I last looked" alone, and that is not a gap to
close: it is shared, so another consumer's scan moves it. The question is only
well-posed relative to a particular looker.

```text
let scan    = fsscan::scan(cache, root, opts, accept).await?;
let changes = scan.changes_since(&load_cursor(pool, SCOPE).await?);
for f in changes.needs_reading() { …; record_file(&mut tx, SCOPE, f).await?; }
```

A scope namespaces cursor rows per `(provider, feed)`, so two feeds can claim
the same file without colliding. Stamping is per file and inside the caller's
transaction, so a crash partway through keeps what landed and re-reads only
the rest. A file stamped again with the same bytes keeps its stamp, so a
source that reads an unchanged file again commits nothing for it.

**A file that is gone takes its records with it.** `changes.removed` names
every path the cursor has and the scan does not. A source whose rows are keyed
by path (a `.vcf` file is one address book, an `.ics` file one calendar) reads
`needs_reading_by_path()`, which counts a moved file as new at its new path,
then deletes the rows of each path in `gone_by_path(&read)` and calls
`forget_file` in the same transaction. The path is removed from the cursor
only then, so a crash in between just retries. `gone_by_path` is empty
whenever the walk reported an error, because a folder that failed to list
looks the same as one whose files were deleted. Report `scan.walk_problems()`
to the run's `RunProblems`, so the skipped deletions show on
the Manage row. Key rows against `scan.given_resolved`, not the configured
path: the scan's paths are resolved, and stripping an unresolved prefix
fails whenever a symlink is in the way.

A source keyed by *content* (SMS backups, Takeout Voice) cannot map
a path to rows, and its files overlap: two exports hold one message. When
`changes.may_have_dropped_records()` (a file removed or rewritten, after a
clean walk) it reads every file, and a run that read every file without a
failure prunes what it did not see (`prune::prune_scope`, then
`prune::delete_owned` for CAS edges) and `forget_files` the removed paths.
A failed read holds the prune back and reports
`Scan::deletions_held_back`. The read costs a pass over every file, but only
on the run where the input shrank.

A feed whose one file is its whole table (Takeout's Maps reviews, YouTube
subscriptions, …) goes through `file_checkpoint::ingest_snapshot`: a
changed file is upserted and the table pruned to what it lists, in one
transaction. A file the parser cannot read as a whole list — no list at
all, or entries none of which it could read — is an error: nothing is
stored or deleted, and the file is not marked read. A file missing from
the scan deletes nothing either: for an export,
a product left out of the request looks exactly like that. Takeout's
folder feeds hold to the same rule one level up (`product_exported`).

**A file as the root is that file.** `scan` of a file walks only that
file's folder, one level deep, and keeps that one name: its `rel` is the
bare file name and its cache key is the one a scan of the folder would
use. A caller that wants a few files side by side — a SQLite file and its
`-wal` — scans the folder with `max_depth: Some(1)` and names them in
`accept`; no folder beside them is opened. The cache read is not
depth-limited: `load_under` still fetches every cached entry under the
folder, which is a database read, not a walk.

**Why content and not `(size, mtime)`.** A cursor on the stat pair
re-ingests a file that was only *touched* (`rsync` without `-t`, a restore
from backup, re-downloading the same export), re-reading and re-parsing the
whole thing though not one byte moved. The cache makes hashing cheap enough
that the cursor can be the content.

**What it does not fix**, because "content hash" invites the wrong assumption:
the cache still decides whether to re-hash from Unison's
`(mtime, size, inode, dev)` cursor, so an edit preserving all four is still
invisible — in one place rather than once per provider.
`an_edit_preserving_the_whole_stat_is_still_invisible` pins it.

**Some files must not be read at all.** A macOS file evicted to iCloud is
"dataless": it has a size and an mtime, and reading one byte silently pulls
the whole thing back over the network. Only the stat can see that, so
`scan_with` takes a veto consulted after the stat and before any read. A
refused file is absent from `files` and leaves the cache untouched, so
nothing later mistakes "we declined to look" for "we looked and it was empty".
It is listed in `present_unread`, as is a file over `max_bytes`: both are
there, so a source keyed by path keeps their rows.
