# lightroom — versioned backup of a SQLite-backed application

Adobe Lightroom Classic keeps its library in a `.lrcat`, which is an
ordinary SQLite database. So does a lot of other desktop software
(Apple Photos, Quicken for Mac, Things, …). This provider mirrors such a
database, table for table, into a doltlite store and lets doltlite's
content-addressed prolly trees do the deduplication.

The engine is
[`datalib_etl_sqlite_mirror`](/datalib/backend/etl/sqlite_mirror/),
shared with `apple_photos`; this provider is the config that points it
at a `.lrcat`, with `id_global` as the preferred key and the `skip_xmp`
preset. This document is where the engine is explained;
[`apple_photos/INGEST.md`](../apple_photos/INGEST.md) covers only what
differs there.

The result is an incremental, versioned backup that costs one pass over
the catalog per run and stores only what actually changed, with every
prior state still queryable. There is no render step; see
[What render will need](#what-render-will-need).

[`CASE_STUDY.md`](CASE_STUDY.md) tells the story of this provider with
measurements, from first principles: what Lightroom and doltlite are,
what the diffs of real backups showed, and where the space goes.

A source reads `catalog.path`, a `.lrcat` or one backup `.zip`,
mirrored again on every run; `backups.path`, a folder of Lightroom's
backups, replayed as the catalog's history; or both, the backups first
and the catalog on top — see [A folder of backups](#a-folder-of-backups).

## The model

```text
drop EVERY table in the mirror
for each SOURCE table:  CREATE TABLE main.t (…);
                        INSERT INTO main.t SELECT … FROM src.t;
dolt_commit
```

That's the whole thing. The drop is unconditional — every mirror table,
not just the ones the source still has. That is what makes a table the
catalog *removed* disappear from HEAD instead of sitting there frozen and
indistinguishable from a live one, and it leaves no "is this one stale?"
question to compute or get wrong.

It looks wasteful and isn't: a table dropped and refilled with the same
schema and rows is no change to doltlite
([doltlite.md § Diffs](/docs/dev/doltlite.md#diffs)), so an ingest of an
unchanged catalog produces **no commit at all** — asserted by
`sqlite_mirror/tests/mirror_roundtrip.rs::unchanged_source_produces_no_commit`,
which was watched failing against a deliberately broken build.

It also means the ingester never has to know how Lightroom marks rows
dirty. Whatever the catalog says today becomes HEAD; history
accumulates behind it.

### The copy runs inside SQLite

doltlite reads plain SQLite files
([doltlite.md § Plain SQLite files](/docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility)),
so the mirror `ATTACH`es the catalog and moves rows with
`INSERT … SELECT`. No value crosses into Rust.

That is much faster (a 3.3 MB, 133-table catalog mirrored in ~220 ms on
doltlite 0.11.50), but the reason it's the right design is fidelity:
SQLite's dynamic typing
survives the hop. A Lightroom column with no declared type holds an
integer in one row and a blob in the next, and both arrive intact.
Marshalling through Rust would force a decision about what such a column
"is" — and getting it wrong would silently corrupt the backup.

The test fixture is minted by `//tests/fixtures:make_lightroom_catalog.py`
in a genrule rather than by Rust, so the input stays independent of the
engine under test.

### A source with nothing in it is refused

SQLite opens a 0-byte file as an empty database, so a catalog truncated
or replaced by a placeholder reads as one with no tables, and the drop
above would empty the mirror. The engine refuses any source that leaves
it no table to mirror — no tables at all, or filters that match none —
before it drops anything (`NothingToMirror`). Run through
`mirror::run_or_report` inside a download's problem collector, that is
a `phase:source` row and the run seals with the mirror as it was;
through `mirror::run`, it fails the run. A file that is not SQLite at
all fails at the `ATTACH` the same way.

### A source that evicts: `append_only`

Some sources keep only a window: Messages with "Keep messages" set to
30 days deletes older messages from `chat.db`. Dropping and refilling
would delete them from HEAD too, when keeping them is the reason to
mirror the file. With `MirrorOptions::append_only` the engine instead
upserts each table: `INSERT OR REPLACE` by its key, so an edit is a
`modified` and a row the source dropped stays; a keyless table adds
only the rows it does not already hold whole, so an edited keyless row
is kept in both versions. No table is dropped, a column the source
gained is added (nullable, since the rows already kept have no value
for it), and one it dropped stays, holding NULL in new rows. A table
whose key would change fails the run before anything is written: the
rows kept are keyed by the old key, and nothing can re-key them. Only
`apple_messages` sets it.

## What gets mirrored

Tables and their rows. Deliberately not mirrored:

| Dropped | Why |
| --- | --- |
| Indexes | A secondary index costs space in every commit and buys a backup nothing. |
| Shadow tables | The backing storage of a virtual table (`<name>_node`, `_content`, …): an index's pages as blobs. The virtual table itself is mirrored, as rows, when the engine has its module (rtree does); one it lacks is skipped with a warning. `PRAGMA table_list` is what tells a shadow table from a real one — `sqlite_master` calls both `table`. A Lightroom catalog has none; a Photos library has an R-tree. |
| Triggers, views | Behavior, not data. The mirror is never written to by an application. |
| CHECK / FOREIGN KEY, collations | `PRAGMA table_info` doesn't surface them, and enforcing the source's integrity rules on a copy of already-valid data buys nothing. |
| Generated columns | No stored value to copy. |
| Column `DEFAULT`s | A rule for writes, and nothing writes to the mirror. See below. |

The schema is rebuilt from `PRAGMA table_xinfo` rather than replayed from
the source's `sqlite_master` text. Replaying verbatim does work — doltlite
0.11.50 parsed all 133 of a stock catalog's table definitions unchanged —
but it forecloses the two things this ingester needs to do to the
schema: drop a column, and choose a different primary key. Both are
textual surgery on arbitrary SQL if you start from the source text, and
neither is if you start from introspection.

Two things introspection hands back are SQL text out of the `.lrcat`, a
file we did not write: the column's declared **type**, and its
**DEFAULT**. Neither is treated as trustworthy.

The type is **quoted**, exactly as the table and column names are.
SQLite's grammar lets a type name be a quoted name — `CREATE TABLE t(a
"my type")` is legal — and `PRAGMA table_xinfo` reports it back with the
quotes gone, so a catalog can declare a type of `INTEGER); DROP TABLE x;
--` and have that text land in the middle of our `CREATE TABLE`. Quoting
closes it, and costs nothing: a quoted type keeps its affinity and a
quoted `"INTEGER"` primary key stays a rowid alias
([doltlite.md § Plain SQLite files](/docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility)).
The mirror's DDL therefore reads `"id_local" "INTEGER"`, which looks
unusual and is exactly equivalent.

A column's `DEFAULT` is not carried across at all. A default only ever
applies to a row inserted without a value for that column, and no such
insert happens here: the mirror names every column on both sides of its
`INSERT … SELECT`, so every mirrored value comes from the source row it
was copied from. A mirrored default could never fire — it would be
decoration on a backup — and reproducing it would mean either trusting
or parsing SQL text out of the `.lrcat`. So it goes the way of the CHECK
constraints and the foreign keys, for the same reason: the mirror copies
data, not the source's rules about writing.

## When the primary key changes

This is the one genuinely tricky part, and it has a clean answer here.

Lightroom keys nearly every table on `id_local INTEGER PRIMARY KEY` — a
rowid alias, which Lightroom is free to renumber on a catalog upgrade or
optimize. Beside it sits `id_global UNIQUE NOT NULL`, a stable UUID. Key
the mirror on `id_local` and a renumbering reads as *every row deleted
and re-added*: a huge, meaningless commit that also costs real space.

So the mirror prefers a **stable key**: a column named in
`stable_key_columns` (default `["id_global"]`) wins over the declared
primary key when the source has a single-column UNIQUE index on it — or,
failing that, when this run finds it distinct and non-NULL in every row
of the table, which is checked with one `COUNT` query per table and
falls back to the declared key with a warning. The second path exists
for Apple Photos, which indexes `ZUUID` without ever declaring it
unique; on a Lightroom catalog the first path always fires. `id_local`
is still mirrored — it is data, just not identity. A renumbering then
reads as one modified column per row.

`sqlite_mirror/tests/mirror_roundtrip.rs::id_local_renumbering_is_a_modification_not_a_churn`
asserts this, and was watched producing
`["added" ×4, "removed" ×4]` against a build with the rewrite disabled
before being believed.

### Every table's outcome, and why

Counts below are measured on the four test catalogs `tests/real_catalogs.rs`
fetches (113 tables each), so you can reproduce them. A different
Lightroom version has a different table count — what carries across is
the rule, not the numbers.

| mirror key | tables | why |
| --- | --- | --- |
| `id_global` | 26 | the source has a single-column UNIQUE index on it, so the rewrite fires |
| `id_local` | 53 | the table has no `id_global` column, and `id_local` is its declared key |
| another declared column | 12 | `image`, `collection`, `fileId`, `version`… — whatever the source declared |
| a UNIQUE index | 20 | the source declares no `PRIMARY KEY`, but a UNIQUE index says what the key is (below) |
| **keyless** | 2 | no `PRIMARY KEY` and no UNIQUE index: `AgLibraryCollectionStackData`, `AgLibraryFolderStackData` |

The middle two rows are the declared-key fallback, and it is *complete*
rather than best-effort: **79 tables carry an `id_local` column, and in
every one of them `id_local` is already the sole declared `PRIMARY
KEY`.** So there is no table where a stable-looking column sits unused —
26 of those 79 also have `id_global` and prefer it, the other 53 keep
`id_local`, and the fallback reaches all of them without a special case.

The 22 tables with no `PRIMARY KEY` are mostly the Adobe cloud-sync
bookkeeping (`AgOzSpaceIds`, `AgPendingOzAssets`, `Migrated*`, …), plus
two stack tables and `AgLibraryImageSyncedAssetData`. Most have no
`id_local` either; their columns are things like `(ozCatalogId,
ozSpaceId)`. Without a key, a table diffs by position, not content
([doltlite.md § Diffs](/docs/dev/doltlite.md#diffs)): the mirror copies
it in the source's scan order, so an unchanged table is no change, but
a row added or deleted in the middle reads as a run of `modified` rows.
On a real set of weekly backups, `AgLibraryImageSyncedAssetData` showed
121,114 of 224,500 rows modified between two of them although the
payload column differed in fewer than 20,000.

But Lightroom does declare those tables' keys, not as a `PRIMARY KEY`
but as a composite UNIQUE index: `index_<Table>_primaryKey` on
`(image, payloadKey)` for `AgLibraryImageSyncedAssetData`, on
`(ozCatalogId, ozSpaceId)` for `AgOzSpaceIds`, and a `UNIQUE (localId,
ozCatalogId)` constraint on `MigratedImages`. So the mirror engine keys
a table that declares no key on its only UNIQUE index, for every source
(Apple Messages' join tables are the same shape). It does so only when
every column of the index is mirrored and no row holds a NULL in it,
since a UNIQUE index lets NULLs repeat and a key does not; a table with
NULLs there stays keyless and the run warns. A table with two UNIQUE
indexes stays keyless too, since neither is more its key than the
other; none of the test catalogs has one. A `primary_keys` entry
or an `id_global` still wins. Every run mirrors the newest state again,
so a store synced before this rule takes it on its next run.

Set `stable_key_columns = []` to mirror declared keys verbatim, or use
`primary_keys = { Table = ["a", "b"] }` to pin one explicitly (an empty
list forces keyless). The override is also the way to give a table a
key the source declares nowhere — but unlike a UNIQUE index it is not
checked: a wrong entry fails the whole run rather than that one table,
because `rebuild_table` creates the table with the key and then does
`INSERT … SELECT`, so a duplicate aborts the ingest.

## The XMP question

`Adobe_AdditionalMetadata.xmp` holds a serialized XMP packet per image
and is routinely the single largest column in a catalog — 486 KB of a
3.3 MB sample, with individual rows up to 72 KB. `AgMetadataSearchIndex`
holds flattened search strings rebuilt from the harvested EXIF/IPTC
tables.

Both are wholly derived from columns that stay, so `skip_xmp = true`
drops them. The column is **absent** from the mirrored table, not blanked
— so it costs nothing in the store and never appears in a diff.

It is **off by default**: a backup should be a faithful mirror unless you
say otherwise. Turn it on when catalog size matters more than being able
to reconstruct the `.lrcat`.

Arbitrary `Table.column` globs work too, via `exclude_columns`.

## Schema evolution

There is no schema-reconciliation logic, because there is nothing to
reconcile: **every run drops every mirrored table and recreates it from
the source.** Discovery is unconditional and per-run, so a growing
catalog needs no configuration at all.

| Source changed | Mirror does | Cost |
| --- | --- | --- |
| New table | creates it | none |
| New column | rebuilds the table with it | none |
| Column removed / retyped / renamed | rebuilds the table without it | none |
| Primary key moved | rebuilds the table on the new key | none |
| Table gone | drops it, so HEAD means "the catalog as it is now" | none |

"None" is the accurate answer in every row, and that is the whole point
of the design: rebuilding is free for the reason in §"The model", so a
rebuild from an unchanged catalog leaves `dolt_status` clean and produces
no commit — which `unchanged_source_produces_no_commit` asserts, for
schema stability as well as row stability.

Diff quality and history are untouched by the rebuild: a keyed row
edited in the source still reads as `modified` in `dolt_diff_<table>`,
and `dolt_history_<table>` keeps every prior version across the drop,
schema changes included.

Nothing compares the mirror's shape with the source's, and nothing can:
doltlite makes a non-`INTEGER` primary key `NOT NULL` where SQLite
reports it nullable
([doltlite.md § Plain SQLite files](/docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility)),
so the two shapes never compare equal.

### Reading a column HEAD no longer has

When the source drops a column, HEAD stops having it, and
`dolt_history_<table>` / `dolt_diff_<table>` project rows through HEAD's
schema — so it is missing from those views too. It is **not** lost.
Open any earlier commit by path and the old schema and values read
straight back
([doltlite.md § Opening a revision by path](/docs/dev/doltlite.md#opening-a-revision-by-path)):

```sh
doltlite -readonly '<db>/<commit-hash>' \
  "SELECT parentId FROM AgLibraryFolder;"   # the column HEAD no longer has
```

Both halves — the absence from `dolt_history_`, and the recovery at the
old commit — are pinned by
`a_dropped_columns_values_survive_at_their_commit` (which recovers by
branching, in one sqlx session).

## Large values

The mirror's `id_global` rewrite is an `INSERT … SELECT` into a table
keyed by text, the shape doltlite corrupted large values in before
0.11.53
([doltlite.md § Versions](/docs/dev/doltlite.md#versions-the-storage-format-and-what-each-pin-brought)).
`large_values_round_trip_byte_for_byte` compares every XMP packet
against the source byte for byte. `hack/doltlite_blob_bug/run.sh`
re-checks upstream: it fetches the doltlite CLI at `MODULE.bazel`'s pin,
or at `DOLTLITE_VERSION`, or runs `DOLTLITE_BIN`, and prints the
`dolt_version()` of the binary it ran.

## Reading a live catalog

Lightroom holds its catalog open, in WAL mode, while running. So by
default each run takes a `VACUUM INTO` snapshot first: that runs inside a
read transaction on the source and includes the WAL's rows
([doltlite.md § Plain SQLite files](/docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility)),
so what lands is one coherent point-in-time copy. It also drops the
freelist, so the snapshot is usually a little smaller than the catalog.

If the read-only open fails — the classic case being a WAL catalog whose
`-shm` file we're not allowed to touch — it falls back to copying the
catalog and its `-wal` / `-shm` / `-journal` sidecars, warns, and carries
on. That copy can be torn if Lightroom writes mid-copy. **Close Lightroom
for a guaranteed-clean backup.**

`snapshot = false` reads the file in place.

## Verified against four real catalogs

`tests/real_catalogs.rs` stacks four real `.lrcat` files onto one store.
They come from
[`thadd3us/lightroom_db_diff`](https://github.com/thadd3us/lightroom_db_diff),
fetched by Bazel rather than vendored (see
[`docs/dev/testing.md`](/docs/dev/testing.md) §"Bazel-fetched test data"),
and they are a chronological progression of one library. What each run
touches, out of 113 tables:

| Catalog | Tables changed | The diff, in words |
| --- | --- | --- |
| `fresh` | 38 | first ingest — the tables that have rows (Lightroom creates all 113 up front; the rest are empty, and an empty table is no data change) |
| `gps_captions_collections_keywords` | 32 | +4 keywords, +2/−1 collections, 2 photos' EXIF modified (the GPS), **0 photos added** |
| `two_more_photos_and_edits` | 46 | **+2 photos**, with their EXIF and IPTC rows |
| `more_face_tags_gps_edit` | 23 | +4 face tags, 3 EXIF rows modified, **0 photos added or removed** |

The tests assert those counts per table, so the diffs have to keep
agreeing with what the catalogs' own filenames claim happened. Re-running
the last catalog rewrites all 358 rows and produces no commit, and the
stacked store is smaller than the four catalogs side by side.

These catalogs are also a **second Lightroom schema version** — 115
`sqlite_master` tables against the 133 of the catalog the design was
first checked on — and `stale_tables_dropped == 0` holds across all
four, since no table disappears between them.

## A folder of backups

Lightroom Classic writes each backup into a folder named for when it was
taken, holding a zip of the catalog:
`Backups/2026-09-27 1650/Lightroom Catalog-v13-3.zip` (older versions
name it `<catalog>.lrcat.zip`). Point `backups.path` at `Backups/` and
each sync mirrors every backup the store does not hold yet, oldest
first, **one commit per backup**. Each backup is mirrored exactly as a
catalog is; any two commits then diff like any two runs.

**HEAD always ends on the newest state.** Every sync ends by mirroring
it again, under the filters it was given, as its last commit: the live
catalog when `catalog.path` is set too, so the backups are the history
and the catalog is HEAD; otherwise the newest backup. When HEAD already
is that state the mirror changes nothing and commits nothing, so
nothing has to be recorded about whether HEAD is behind. That keeps
things simple when a backup turns up late, older than what is already
committed: it is replayed like any other, the history detours back to
it for one commit, and the next commit returns to the present. A run
that fails or is stopped before that last commit leaves HEAD behind, and
the next sync puts it right.

When the newest backup the store holds is no longer in the folder, it
cannot be mirrored again, and mirroring an older one on top would take
its state out of HEAD; so HEAD stays where the last sync left it, and
that backup is a problem on the Manage row until a newer one arrives.

- **Which file is a backup.** The folder is scanned with `fsscan`. A
  backup is known by its hash (below), so every run needs every
  backup's hash; the host's fingerprint cache answers that with a
  `stat` for a file it has hashed before. Each entry in the folder is
  one backup: a folder with a catalog in it, or a catalog file on its
  own. Its time comes from the start of its name (`YYYY-MM-DD HHMM`),
  so a note added after it (`2019-12-14 0731 - Before restoring
  captions`) is fine. When a folder has both the `.zip` and an unpacked `.lrcat`, the
  zip is used: it is what Lightroom wrote, and the unpacked copy may
  have been opened since. An entry with no catalog in it is ignored; a
  folder holding two catalogs, or one whose name does not start with a
  date, is reported as a problem on the Manage row.
- **A backup is known by its bytes, not its name.** Renaming a backup's
  folder by hand — adding a note — changes nothing: its hash is already
  in the store. A backup whose file changed after it was committed is
  new bytes, so it is replayed.
- **Each commit names its file and is dated when its backup was taken.**
  The message's first line is the backup's file, relative to the folder
  (`download lightroom: backup 2026-09-27 1650/Lightroom Catalog-v13-3.zip`),
  and the mirror's counts follow below it. The date is the folder's time
  (`dolt_commit('--date', …)`) read in this machine's time zone, so
  `dolt_history_<table>.commit_date` reads as the catalog's own history.
  The catalog's commits, a backup mirrored again to go back on top, and
  the store's own bookkeeping commits are dated when they were made.
- **`lightroom_snapshots` lists the backups the store holds**: `snapshot`
  (the entry's name), `taken_at` (from the name, local time), `file`
  (relative to the folder) and `blake3` (the file's hash), each row
  landing in the commit that mirrored it.
- **A changed filter reaches HEAD without waiting for a backup.**
  `include_tables`, `exclude_tables`, `exclude_columns`, `skip_xmp`,
  `stable_key_columns` and `primary_keys` shape every mirror from then
  on, and the sync's last commit is the newest state mirrored under
  them. Earlier commits keep the filters they were made with.
- **A folder with no backups fails the run**, as does one that cannot
  be read (a backup drive that is not mounted), and so does a
  `catalog.path` that is not there. An entry inside the folder the walk
  could not read is a `listing:backups` row, a backup it found and could
  not open a `record:backups:<path>` row, and the rest is mirrored.
- **A backup that will not mirror is a problem on that backup**, keyed
  `record:lightroom_snapshots:<entry name>` — a zip that will not open,
  a catalog that is not one — and the backups after it are still
  mirrored. It is not in `lightroom_snapshots`, so every sync tries it
  again, replaying it like a late backup once it mirrors, and HEAD ends
  on the newest backup that did mirror. The exception is a failure
  after the mirror engine has emptied the mirror's tables: committing
  anything on top of that would publish half a catalog, so that fails
  the run, and the next run's open discards the half-written state.
- **A stopped run clears no problems**, so the last complete run's
  stand; a backup that would not mirror before the stop is still
  recorded.

A zip is unpacked into a temporary directory for the length of its
mirror, so a run needs free space for one catalog at a time; the
unpacked copy is read without a snapshot, since nothing else has it
open. `tests/backups_folder.rs` covers the order, the dates, the
messages, the ledger, a late older backup, the filter change, the live
catalog on top, an unchanged catalog committing nothing, a HEAD left
behind put right, a deleted newest backup, backups known by their
bytes, a backup that will not mirror and a stopped run, against zipped
copies of the TNG catalog.

## Store size and `gc`

An uncollected store grows every run, a no-op run included
([doltlite.md § Disk space and `dolt_gc`](/docs/dev/doltlite.md#disk-space-and-dolt_gc)).
Measured on a real 3.1 MB catalog (133 tables, 1949 rows, 50 images),
after a few runs' history, on doltlite 0.11.50:

| | Size |
| --- | --- |
| The `.lrcat` itself | 3.1 MB |
| Mirror, collected | **1.4 MB** |
| Mirror, collected, `skip_xmp` | **812 KB** |
| Mirror, *not* collected | 4.0 – 5.2 MB, growing per run |

The same holds at scale. A backups folder of ten catalogs (0.9 to 6.0 GB
each, about 32 GiB in all; the newest 135 tables and 5.4 million rows),
ingested in date order and never collected:

| | Size |
| --- | --- |
| Mirror, *not* collected | **12.38 GiB** (13,293,589,085 bytes) |
| Same file after `dolt_gc()` | **7.86 GiB** (8,443,844,496 bytes) |
| Chunks removed | 783,443 of 1,756,444 (45%) |
| Time to collect, Apple Silicon laptop | 2 min 41 s |

That is a third of the file, taking the store from about 0.4 of the
source's size to about 0.25. Collecting a copy of the file is a safe way to
measure it without touching the store.

`gc = true` runs it at the start of each run, which collects the
*previous* run's garbage. Same steady-state result, and it happens while
the working tree is provably clean and outside the commit lifecycle the
orchestrator owns — but it does mean a brand-new store isn't collected
until its second run.
It is **off by default** because gc rewrites the whole file, which is
time a routine no-op run shouldn't spend. It runs once per sync, before
the sync's first mirror, and every sync mirrors at least the newest
state, so with `gc = true` every sync collects. Running it by hand
periodically, with no sync running, is a fine alternative:

```sh
bazelisk build //third-party/doltlite:doltlite
bazel-bin/third-party/doltlite/doltlite <root>/lightroom/ingest/entities.doltlite_db "SELECT dolt_gc();"
```

## Why a re-ingest grew the store, and how to find out

A second ingest of a newer catalog adds a commit, and the file grows by
the rows that commit had to store again. Which tables those are can be
read from the store's metadata alone, without looking at a single row's
contents. Take the two `download` commits from `dolt_log`, then:

```sh
D=datalib-doltlite   # or the doltlite build from the section above
DB=<root>/lightroom/ingest/entities.doltlite_db
$D -readonly $DB "SELECT commit_hash, substr(message,1,140), date FROM dolt_log ORDER BY date;"

# which tables changed, and how much (counts only)
$D -readonly $DB "SELECT table_name, rows_unmodified, rows_added, rows_deleted, rows_modified, cells_modified
                    FROM dolt_diff_stat('<old>', '<new>')
                   ORDER BY rows_added+rows_deleted+rows_modified DESC LIMIT 40;"

# for one table with lots of modified rows: which column changed, and did its storage type change?
$D -readonly $DB "SELECT count(*), sum(from_c IS NOT to_c), sum(typeof(from_c) IS NOT typeof(to_c))
                    FROM dolt_diff_<table>('<old>', '<new>') WHERE diff_type = 'modified';"
```

`cells_modified / rows_modified` is the quickest signal: near 1.0 means
one column changed in every row. `pragma_table_info('<table>')` lists
the columns without reading data. The row-level diff is a per-table
vtab, `dolt_diff_<table>('<from>', '<to>')`, not the three-argument
`dolt_diff(...)`
([doltlite.md § Diffs](/docs/dev/doltlite.md#diffs)).

What three catalogs from different Lightroom generations (2016, 2018,
2019) showed:

- **New photos are cheap and honest.** Each new image is one added row
  in every per-image table (`Adobe_images`, `AgLibraryFile`,
  `Adobe_imageDevelopSettings`, …) and a few modified ones. The row
  counts add up exactly: old rows + added − deleted = new rows.
- **A change of storage type rewrites every row of that column.**
  Between the first two catalogs, `Adobe_libraryImageDevelopHistoryStep.text`
  went from `text` to `blob` in every row while its `digest` and `name`
  stayed the same, and the values got shorter. That fits Lightroom
  compressing the value in newer versions; the format was not identified.
  The mirror copies what the source holds, so the whole column is stored
  again once. The next catalog with the same format stored no rewrite.
- **Reprocessing rewrites rows too.** The face tables
  (`Adobe_libraryImageFaceProcessHistory`, `AgLibraryFace`,
  `AgLibraryFaceData`) had nearly every row modified in one column when
  the face model changed. That is real change in the source, not churn.
- **A stable key is why this reads as an edit.** `id_global` keys most
  tables, so a changed column is a modified row rather than a delete and
  an add ([When the primary key changes](#when-the-primary-key-changes)).
  Keyless tables compare by position, so they show as wholly modified
  whenever rows are added; `AgLibraryImageSyncedAssetData` did before the
  mirror keyed it on its unique index.
- **The size ratio is not the diff.** A catalog that shrank in the source
  still grew the store, because a store keeps every version. And a
  quieter diff is not a smaller file: keying the sync tables cut the
  rows modified between weekly backups by about 97% and changed the file
  by under 0.01%, because those tables are small. On the ten-catalog
  store above the size was history plus uncollected garbage, and
  `dolt_gc` (above) reclaimed a third of it.

The counts say *what* changed; only the values say *why*, and this
recipe deliberately does not read them. The causes above are inferences
from column names, types and lengths.

## Running it

As a DAG step — see the `lightroom` stanza in
[`docs/user/config_examples/all_sources.toml`](/docs/user/config_examples/all_sources.toml):

```toml
[[groups]]
id = "lightroom"
type = "lightroom"

[[steps]]
group = "lightroom"
function = "ingest"
[steps.params.catalog]
path = "~/Pictures/Lightroom/Lightroom Catalog-v14.lrcat"
```

and, for a folder of backups beside it or instead of it,
`[steps.params.backups]` with `path = "~/Pictures/Lightroom/Backups"`.

Or standalone, with `--catalog` (a `.lrcat` or a `.zip`), `--backups`,
or both:

```sh
bazelisk build //datalib/backend/etl/providers/lightroom:lightroom_ingest
bazel-bin/datalib/backend/etl/providers/lightroom/lightroom_ingest \
  --catalog ~/Pictures/Lightroom/Catalog.lrcat \
  --db ~/backups/lightroom.doltlite_db
```

## Reading the backup

Use the doltlite shell ([`docs/dev/doltlite.md`](/docs/dev/doltlite.md)
has where to get it and the general recipes):

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=~/backups/lightroom.doltlite_db

# What has this backup captured?
$dl -readonly $db "SELECT commit_hash, date, message FROM dolt_log;"

# What changed in the latest run, and in which tables?
$dl -readonly $db "SELECT table_name FROM dolt_diff WHERE commit_hash = 'abc123…';"

# Which photos were re-rated?
$dl -readonly $db "SELECT to_id_global, from_rating, to_rating FROM dolt_diff_Adobe_images
         WHERE diff_type = 'modified' AND to_commit = 'abc123…';"

# Every value a photo's row has ever held.
$dl -readonly $db "SELECT commit_date, rating, pick FROM dolt_history_Adobe_images
         WHERE id_global = '49AFB3AB-…' ORDER BY commit_date;"

# A photo deleted from the catalog months ago.
$dl -readonly $db "SELECT * FROM dolt_history_Adobe_images WHERE id_global = '…';"
```

`dolt_history_<table>` carries the full row at every commit, which is
how a deleted photo's metadata is recovered. One table as it was at a
commit is `dolt_at_<table>('<commit-ish>')`.

To see a whole catalog as it was — including columns or tables that HEAD
no longer has — open that commit by path; it is read-only and writes
nothing to the store:

```sh
$dl -readonly "$db/abc123…" "SELECT COUNT(*) FROM Adobe_images;"
```

## Scaling caveat

Each table is filled inside its own transaction and the run commits
once, at the end, so a crash mid-run leaves HEAD untouched and a dirty
working set that the next open discards
([doltlite.md § A writer's open discards the working set](/docs/dev/doltlite.md#a-writers-open-discards-the-working-set));
the next run refills from the source as every run does.

Peak memory is about twice the rows copied
([doltlite.md § What a write costs](/docs/dev/doltlite.md#what-a-write-costs)).
A multi-hundred-GB database would want the copy chunked by primary-key
range; a Lightroom catalog (tens of MB) is nowhere near that.

## What render will need

Render is not built: a photo is not chat-shaped and the projection is
its own design question. What it would need:

- **One `grid_rows` row per image.** This join runs against a mirrored
  catalog today and yields absolute on-disk paths:

  ```sql
  SELECT i.id_global,
         i.captureTime,
         i.rating,
         rf.absolutePath || f.pathFromRoot || fi.baseName || '.' || fi.extension AS path,
         cm.value AS camera,
         ip.caption
    FROM Adobe_images i
    JOIN AgLibraryFile       fi ON fi.id_local = i.rootFile
    JOIN AgLibraryFolder      f ON f.id_local  = fi.folder
    JOIN AgLibraryRootFolder rf ON rf.id_local = f.rootFolder
    LEFT JOIN AgHarvestedExifMetadata     e ON e.image    = i.id_local
    LEFT JOIN AgInternedExifCameraModel  cm ON cm.id_local = e.cameraModelRef
    LEFT JOIN AgLibraryIPTC              ip ON ip.image    = i.id_local;
  ```

  Add `AgLibraryKeywordImage` → `AgLibraryKeyword` for keywords and
  `AgInternedExifLens` for the lens. `Adobe_images.captureTime` is
  ISO-8601 but **carries no offset** (`2002-10-01T00:00:00`); it stays
  as written ([timestamp convention](/AGENTS.md#timestamp-convention)),
  but a sortable UTC twin cannot be derived from it alone.
- **A way to show the pictures.** The catalog stores paths, not pixels.
  Three candidates, cheapest first: (1) link out to the original file via
  the resolved absolute path — no bytes copied, breaks if the library
  moves; (2) pull Lightroom's own previews out of the sibling
  `<Catalog> Previews.lrdata` bundle into the blob CAS — self-contained
  and already grid-sized, but the bundle's layout is undocumented and
  hasn't been examined here, so cost that out before committing to it;
  (3) generate thumbnails from the originals — most work, most control.
- **`fsindex` as a companion**, if you want to know whether the files the
  catalog points at are still there.
