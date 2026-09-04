# lightroom — versioned backup of a SQLite-backed application

Adobe Lightroom Classic keeps its library in a `.lrcat`, which is an
ordinary SQLite database. So does a lot of other desktop software
(Quicken for Mac, Apple Photos, Things, …). This provider mirrors such a
database, table for table, into a doltlite store and lets doltlite's
content-addressed prolly trees do the deduplication.

The result is an incremental, versioned backup that costs one pass over
the catalog per run and stores only what actually changed, with every
prior state still queryable.

**Status: download-only prototype.** There is no render step yet — see
[What render will need](#what-render-will-need).

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

It looks wasteful and isn't: doltlite stores a table as a content-
addressed prolly tree, so a `CREATE TABLE` identical to the one at HEAD
is not a change, and a row written back byte-identical produces the same
chunk and lands in the same place. Drop a table, recreate it, refill it
with the same 419 rows, and `dolt_status` comes back **clean**.

Which means an ingest of an unchanged catalog produces **no commit at
all** — verified against a real catalog and asserted by
`tests/mirror_roundtrip.rs::unchanged_source_produces_no_commit`, which
was watched failing against a deliberately broken build before being
believed.

It also means the ingester needs no cursor, no watermark and no
change-tracking of its own. It never has to know how Lightroom marks
rows dirty. Whatever the catalog says today becomes HEAD; history
accumulates behind it.

### The copy runs inside SQLite

doltlite's amalgamation reads *and writes* ordinary SQLite files as well
as `.doltlite_db` ones, so the mirror `ATTACH`es the catalog and moves
rows with `INSERT … SELECT`. No value crosses into Rust.

That is much faster (a 3.3 MB, 133-table catalog mirrors in ~220 ms), but
the reason it's the right design is fidelity: SQLite's dynamic typing
survives the hop. A Lightroom column with no declared type holds an
integer in one row and a blob in the next, and both arrive intact.
Marshalling through Rust would force a decision about what such a column
"is" — and getting it wrong would silently corrupt the backup.

The one thing doltlite *cannot* do is create a plain SQLite file: a
database it creates is always in its own format. That's why the test
fixture is minted by `//tests/fixtures:make_lightroom_catalog.py` in a genrule rather
than by Rust.

## What gets mirrored

Tables and their rows. Deliberately not mirrored:

| Dropped | Why |
| --- | --- |
| Indexes | doltlite keys each table by its primary key in a prolly tree. A secondary index costs space in every commit and buys a backup nothing. |
| Triggers, views | Behavior, not data. The mirror is never written to by an application. |
| CHECK / FOREIGN KEY, collations | `PRAGMA table_info` doesn't surface them, and enforcing the source's integrity rules on a copy of already-valid data buys nothing. |
| Generated columns | No stored value to copy. |
| Non-literal `DEFAULT`s | A default that is an expression rather than a literal — `(datetime('now'))` — is dropped, with a warning. Every mirrored column is written explicitly, so no mirrored row would have taken it. See below. |

The schema is rebuilt from `PRAGMA table_xinfo` rather than replayed from
the source's `sqlite_master` text. Replaying verbatim does work —
doltlite parses all 133 of a stock catalog's table definitions unchanged
— but it forecloses the two things this ingester needs to do to the
schema: drop a column, and choose a different primary key. Both are
textual surgery on arbitrary SQL if you start from the source text, and
neither is if you start from introspection.

Introspection reports two things that are not identifiers, so quoting
them on the way back into the mirror's `CREATE TABLE` would change their
meaning rather than make them safe: a column's declared type and its
`DEFAULT`. Both come out of the `.lrcat`, which is a SQLite file we did
not write — and SQLite lets a *quoted* type name contain anything at
all, which `PRAGMA table_xinfo` then reports back with the quotes gone.
So both are checked instead.

A declared type has to be a plain type name: letters, digits,
underscores and spaces, optionally followed by one or two numbers in
parentheses. That covers everything SQLite documents (`INTEGER`,
`VARCHAR(255)`, `UNSIGNED BIG INT`, `NUMERIC(10,5)`). Anything else
fails the run, and the way past it is to skip that column with
`exclude_columns`. Failing is deliberate: dropping the type instead
would leave the column untyped, which changes its affinity and so what
the mirror stores — a quiet narrowing rather than a loud stop.

A `DEFAULT` has to be a literal: a number, a quoted string, a blob,
`NULL`, `TRUE`/`FALSE`, or one of the `CURRENT_*` keywords. Anything
else is dropped with a warning rather than failing the run, because a
dropped default changes nothing observable here — `copy_sql` names every
column, so no mirrored row is ever filled in from one.

## When the primary key changes

This is the one genuinely tricky part, and it has a clean answer here.

Lightroom keys nearly every table on `id_local INTEGER PRIMARY KEY` — a
rowid alias, which Lightroom is free to renumber on a catalog upgrade or
optimize. Beside it sits `id_global UNIQUE NOT NULL`, a stable UUID. Key
the mirror on `id_local` and a renumbering reads as *every row deleted
and re-added*: a huge, meaningless commit that also costs real space.

So the mirror prefers a **stable key**: any single-column UNIQUE index
whose column is named in `stable_key_columns` (default `["id_global"]`)
wins over the declared primary key. `id_local` is still mirrored — it is
data, just not identity. A renumbering then reads as one modified column
per row.

`tests/mirror_roundtrip.rs::id_local_renumbering_is_a_modification_not_a_churn`
asserts this, and was watched producing
`["added" ×4, "removed" ×4]` against a build with the rewrite disabled
before being believed.

On a stock catalog, 27 of 133 tables get the rewrite. The rest fall
through:

- declared key, when there's no stable candidate (`AgLibraryKeywordImage`
  has `id_local` and nothing else);
- **keyless**, when the source table has no key either (`AgOzSpaceIds`).
  doltlite versions keyless tables by row multiset, which is the honest
  representation of a table that has no identity of its own.

Set `stable_key_columns = []` to mirror declared keys verbatim, or use
`primary_keys = { Table = ["a", "b"] }` to pin one explicitly (an empty
list forces keyless).

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
of the design. Rebuilding is free because doltlite is content-addressed:
a `CREATE TABLE` identical to the one at HEAD is not a change, and rows
written back byte-identical hash to the chunks already there. So a
rebuild from an unchanged catalog leaves `dolt_status` clean and produces
no commit — which `unchanged_source_produces_no_commit` asserts, and
which now covers schema stability as well as row stability in one
assertion.

Diff quality and history are untouched by the rebuild: an edited row
still reads as `modified` (not removed-plus-added) in
`dolt_diff_<table>`, because dolt matches rows by primary key and neither
knows nor cares that the table was dropped in between, and
`dolt_history_<table>` keeps every prior version across the drop —
including across a schema change.

An earlier version of this provider compared the mirror's introspected
shape against the source's and chose between `ALTER TABLE … ADD COLUMN`
and drop-and-recreate. It was ~150 lines, and it had a bug this version
cannot have: SQLite reports a non-`INTEGER PRIMARY KEY` column as
nullable while dolt stores it NOT NULL, the two shapes therefore never
compared equal, and ten of a real catalog's 133 tables rebuilt on *every*
run. Nothing compares shapes now.

### Reading a column HEAD no longer has

When the source drops a column, HEAD stops having it, and
`dolt_history_<table>` / `dolt_diff_<table>` project rows through HEAD's
schema — so it is missing from those views too. It is **not** lost.
Branch at any earlier commit and the old schema and values read straight
back:

```sql
SELECT dolt_branch('before_drop', '<commit-hash>');
SELECT dolt_checkout('before_drop');
SELECT parentId FROM AgLibraryFolder;   -- the column HEAD no longer has
```

Both halves — the absence from `dolt_history_`, and the recovery via a
branch — are pinned by
`a_dropped_columns_values_survive_at_their_commit`.

## A doltlite blob bug this provider found (fixed upstream)

**doltlite used to silently corrupt large values read out of an ordinary
SQLite file** — [dolthub/doltlite#2327](https://github.com/dolthub/doltlite/issues/2327),
fixed in **v0.11.53** by
[dolthub/doltlite#2329](https://github.com/dolthub/doltlite/pull/2329),
which is what `MODULE.bazel` pins. It is written up here because the
failure mode is worth recognising, not because the mirror still has to
dodge it.

Any value that spilled past the source file's local payload limit (4057
bytes at the usual 4096-byte `page_size`; the cutover moved with
`page_size`) was backed by a buffer reused across rows, so any consumer
holding more than one row's value at a time — `DISTINCT`, `GROUP BY`, a
materialised subquery, or an `INSERT … SELECT` into a table whose primary
key is not a rowid alias — saw every value collapse onto the first row's
bytes, truncated to each row's own correct length. Row counts, lengths
and `typeof()` all still looked right; no error was raised; the damage
survived `dolt_commit`. A plain row-at-a-time scan was unaffected, as
were doltlite's own tables.

This provider walked straight into it, and only because of the
`id_global` key rewrite: keying on the source's `id_local INTEGER` is the
shape that happened to be safe. Six of a real catalog's 50 XMP packets
came out holding another photo's bytes. Every cheap check passed — it
surfaced only on a byte-for-byte comparison against the source, and the
first version of *that* check was an `EXCEPT` query against the ATTACHed
catalog, which hit the same bug and lied.

Until the fix landed, [`mirror::rebuild_table`](src/download/mirror.rs)
routed rows through a keyless staging table whenever the mirror's key was
not a single `INTEGER` column. That detour is gone;
`large_values_round_trip_byte_for_byte` is the regression test that
justified it and now guards its absence, comparing every XMP packet
against the source byte for byte.

To re-check the upstream behaviour directly, without going through this
crate:

```sh
hack/doltlite_blob_bug/run.sh                    # the pinned-fix version
DOLTLITE_VERSION=0.11.52 hack/doltlite_blob_bug/run.sh   # the last broken one
```

That script is standalone — it fetches the official doltlite CLI, builds a
plain SQLite file with the system `sqlite3`, and needs nothing from this
repo. `DOLTLITE_BIN=… run.sh` points it at a local build instead.

## Reading a live catalog

Lightroom holds its catalog open, in WAL mode, while running. So by
default each run takes a `VACUUM INTO` snapshot first: that runs inside a
read transaction on the source, so what lands is one coherent
point-in-time copy. It also drops the freelist, so the snapshot is
usually a little smaller than the catalog.

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

## Store size and `gc`

doltlite accumulates unreachable chunks as history is written, so an
un-collected store looks larger than it is. Measured on a real 3.1 MB
catalog (133 tables, 1949 rows, 50 images), after a few runs' history:

| | Size |
| --- | --- |
| The `.lrcat` itself | 3.1 MB |
| Mirror, collected | **1.4 MB** |
| Mirror, collected, `skip_xmp` | **812 KB** |
| Mirror, *not* collected | 4.0 – 5.2 MB, growing per run |

`dolt_log` and `dolt_history_*` are intact in every collected case —
collection reclaims unreachable chunks, not history.

`gc = true` runs it at the start of each run, which collects the
*previous* run's garbage. Same steady-state result, and it happens while
the working tree is provably clean and outside the commit lifecycle the
orchestrator owns — but it does mean a brand-new store isn't collected
until its second run.
It is **off by default** because it rewrites the whole chunk store, which
is time a routine no-op run shouldn't spend. Running it by hand
periodically is a fine alternative:

```sh
bazelisk build //third-party/doltlite:doltlite
bazel-bin/third-party/doltlite/doltlite <root>/lightroom/raw/entities.doltlite_db "SELECT dolt_gc();"
```

## Running it

As a DAG step — see the `lightroom` stanza in
[`docs/user/config_examples/all_sources.toml`](/docs/user/config_examples/all_sources.toml):

```toml
[[steps]]
id = "lightroom.download"
command = "datalib-step download lightroom"
outputs = ["lightroom/raw"]
[steps.params.common]
input_path = "~/Pictures/Lightroom/Lightroom Catalog-v14.lrcat"
```

Or standalone:

```sh
bazelisk build //datalib/backend/etl/providers/lightroom:lightroom_ingest
bazel-bin/datalib/backend/etl/providers/lightroom/lightroom_ingest \
  --catalog ~/Pictures/Lightroom/Catalog.lrcat \
  --db ~/backups/lightroom.doltlite_db
```

## Reading the backup

Stock `sqlite3` cannot open a `.doltlite_db`. Use the Bazel-built shell
(see [`docs/dev/doltlite.md`](/docs/dev/doltlite.md)):

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=~/backups/lightroom.doltlite_db

# What has this backup captured?
$dl $db "SELECT commit_hash, date, message FROM dolt_log;"

# What changed in the latest run, and in which tables?
$dl $db "SELECT table_name FROM dolt_diff WHERE commit_hash = 'abc123…';"

# Which photos were re-rated?
$dl $db "SELECT to_id_global, from_rating, to_rating FROM dolt_diff_Adobe_images
         WHERE diff_type = 'modified' AND to_commit = 'abc123…';"

# Every value a photo's row has ever held.
$dl $db "SELECT commit_date, rating, pick FROM dolt_history_Adobe_images
         WHERE id_global = '49AFB3AB-…' ORDER BY commit_date;"

# A photo deleted from the catalog months ago.
$dl $db "SELECT * FROM dolt_history_Adobe_images WHERE id_global = '…';"
```

`dolt_history_<table>` is the one to reach for day to day: it carries the
full row at every commit, which is how a deleted photo's metadata is
recovered. There is no MySQL-style `AS OF` clause in doltlite — the
equivalent is the table-valued `dolt_at_<table>('<commit-ish>')`, which
accepts `HEAD`, `HEAD~N` or a raw commit hash and reads *committed*
state only (it ignores a dirty working set).

To see a whole catalog as it was — including columns or tables that HEAD
no longer has — branch at the commit and check it out. The active branch
is per-connection, so do it in one session:

```sh
$dl $db <<'SQL'
SELECT dolt_branch('march', 'abc123…');
SELECT dolt_checkout('march');
SELECT COUNT(*) FROM Adobe_images;
SQL
```

## Scaling caveat

Each table is filled inside its own transaction, so peak memory scales
with the largest single table rather than the whole catalog. That split
is not stylistic: doltlite holds a transaction's writes in memory at
roughly 3–4× the data size, so wrapping a whole run in one transaction
costs ~510 MB peak RSS for 150 MB of rows — fine at that size, ~15 GB for
a 4–5 GB catalog, which is not.

The run is still atomic *as history*: the dolt commit only happens at the
end, so a crash mid-run leaves HEAD untouched and a dirty working tree,
which `doltlite_raw::open` seals into its own rescue commit next time. A multi-hundred-GB database
would want the copy chunked by primary-key range. A Lightroom catalog
(tens of MB, low hundreds of thousands of rows) is nowhere near that.

## What render will need

Render is deferred rather than stubbed, because a photo is not
chat-shaped and the projection is its own design question. What it will
need:

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
  `AgInternedExifLens` for the lens. One caveat for
  [the repo's timestamp convention](/AGENTS.md#timestamp-convention):
  `Adobe_images.captureTime` is ISO-8601 but **carries no offset**
  (`2002-10-01T00:00:00`), so render will have to decide what to do about
  that rather than pass it straight through.
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

Once a second SQLite-backed source lands (Quicken, say), lift
`download/mirror.rs` + `download/plan.rs` into `datalib_etl` and let both
provider crates depend on it. Keeping it in this crate until then avoids
inventing a shared abstraction from a single example.
