# apple_photos — versioned backup of an Apple Photos library's database

Apple Photos keeps everything it knows about a library — every asset,
album, person, face, keyword, edit and iCloud state — in
`<library>.photoslibrary/database/Photos.sqlite`, an ordinary SQLite
database with a Core Data schema. This provider mirrors that file into
a doltlite store with the engine `lightroom` uses,
[`datalib_etl_sqlite_mirror`](/datalib/backend/etl/sqlite_mirror/), and
so inherits everything
[`lightroom/INGEST.md`](../lightroom/INGEST.md) says about the model:
drop and refill every table each run, let doltlite's content-addressed
storage keep only what changed, and get `dolt_log` /
`dolt_history_<table>` / `dolt_diff_<table>` over the result. **Read
that document first.** This one covers only what is Photos-shaped.

**Status: download-only.** Nothing is rendered; see [What render will
need](#what-render-will-need).

## What a library looks like

Measured on an 11-photo library written by macOS 26 (`LibrarySchemaVersion`
5001 in `database/DataModelVersion.plist`):

| | |
| --- | --- |
| tables | 89, plus 434 indexes and 22 triggers |
| `ZASSET` | one row per photo or video, 135 columns |
| 1:1 side tables of `ZASSET` | `ZADDITIONALASSETATTRIBUTES` (original filename, title, time zone), `ZEXTENDEDATTRIBUTES` (EXIF), `ZMEDIAANALYSISASSETATTRIBUTES`, `ZPHOTOANALYSISASSETATTRIBUTES`, … |
| albums | `ZGENERICALBUM`, with `Z_33ASSETS` as the album↔asset join (the digits are Core Data's entity numbers and change between schema versions) |
| people and faces | `ZPERSON`, `ZDETECTEDFACE`, `ZFACECROP` |
| the geo index | `Z_RT_Asset_boundedByRect`, an R-tree virtual table over each asset's latitude/longitude |
| the file | `originals/<ZDIRECTORY>/<ZFILENAME>` — `originals/E/EE1CACED-….jpeg`; the name it had when imported is `ZADDITIONALASSETATTRIBUTES.ZORIGINALFILENAME` |

A different macOS version has a different table count and different
join-table digits. The mirror does not care: every run discovers the
schema afresh, which is the engine's whole design.

## Keys: `Z_PK` is a rowid, `ZUUID` is the identity

Core Data keys every entity table on `Z_PK INTEGER PRIMARY KEY` — a
rowid alias, which Photos renumbers on a library repair or an iCloud
re-sync. Beside it, 32 tables carry `ZUUID`, the stable identifier
Photos itself uses (it is the name of the file under `originals/`).

Lightroom's `id_global` is declared `UNIQUE NOT NULL`, and the engine's
original rule keyed on a stable column only when the source had a
single-column UNIQUE index on it. **Photos never declares `ZUUID`
UNIQUE** — `ZASSET` has a plain `Z_Asset_byUuidIndex` and nothing more —
so under that rule every Photos table would have keyed on the rowid.

The engine now has a second way to accept a stable column: on each run,
for every table where a `stable_key_columns` entry is present but not
declared UNIQUE, it counts rows, distinct values and non-NULL values,
and keys on the column when all three agree. Where they do not — a
table whose `ZUUID` is NULL for some rows — it keeps the declared key
and says so with a warning, rather than either lying or failing the
run. On the sample library 23 tables key on `ZUUID` this way
(`stable_keys=23` in the run summary), and none warn.

The reason it matters is the same as for Lightroom: a `Z_PK`
renumbering then reads as one modified column per row instead of every
row removed and re-added, and a photo's history stays one line in
`dolt_history_ZASSET` across it.
`tests/photos_roundtrip.rs::a_z_pk_renumbering_is_a_modification_not_a_churn`
asserts this, and `zuuid_is_the_key_wherever_it_holds` asserts the
NULL fallback against a fixture table built to need it.

The join tables (`Z_33ASSETS` and its siblings) are keyed on pairs of
rowids by the source, and stay that way: there is no UUID to rekey them
on, and a renumbering shows up there as removed-plus-added. That is the
honest representation.

## `skip_history`: what moves when nothing happened

Photos is never idle. `photolibraryd` and `photoanalysisd` keep the
database open and keep writing to it, and Core Data records every save
in its persistent history. Measured across one editing session on the
sample library (two favourites toggled, three imports, one album):

| table | rows changed | what it is |
| --- | --- | --- |
| `ACHANGE` | +86 | Core Data persistent history: one row per changed object per save |
| `ATRANSACTION` | +16 | one row per save |
| `ZBACKGROUNDJOBWORKITEM` | +24, −1 | the daemons' work queue |
| `Z_PRIMARYKEY` | 18 modified | Core Data's per-entity high-water marks |
| every touched row | `Z_OPT` bumped | Core Data's optimistic-locking version counter |

Against that, the edit itself was `ZFAVORITE` and `ZMODIFICATIONDATE`
on two `ZASSET` rows, three new assets with their side-table rows, and
one album.

`skip_history = true` — the default — excludes the tables in
`HISTORY_TABLE_PATTERNS` (`ACHANGE`, `ATRANSACTION`,
`ATRANSACTIONSTRING`, `ZBACKGROUNDJOBWORKITEM`, `Z_PRIMARYKEY`,
`Z_METADATA`, `Z_MODELCACHE`) and the `Z_OPT` column of every table.
The persistent history is a change log; the mirror's `dolt_log` *is* a
change log, with the actual before-and-after values rather than
Core Data's tombstones. The rest says nothing about a photo.

It defaults **on**, unlike lightroom's `skip_xmp`, because the
alternative is not "a faithful mirror" but "a commit on every run":
with those tables in, a library nobody has opened still changes between
any two runs, and the engine's central property — an unchanged source
produces no commit — is false.
`a_favorite_toggle_is_one_modified_row_and_daemon_churn_is_nothing`
pins both halves: daemon writes alone commit nothing, and the favourite
is then exactly one modified row.

What is deliberately **not** excluded: the auto-curation relationship
columns (`ZASSET.Z*HIGHLIGHTBEING*`, `ZDAYGROUPHIGHLIGHT…`), which
Photos reshuffles as it builds Memories. They are noisy — five of the
six `ZASSET` rows modified in the session above moved only in those —
but they are real state of the library rather than bookkeeping, and
`exclude_columns = ["ZASSET.*HIGHLIGHTBEING*"]` is one line if you
disagree.

## The R-tree, and why shadow tables are skipped

`Z_RT_Asset_boundedByRect` is `CREATE VIRTUAL TABLE … USING RTREE`. The
doltlite amalgamation has the rtree module compiled in, so the engine
reads it like any table and writes its rows to a plain table of the
same name.

Under it sit three **shadow tables** — `_node`, `_parent`, `_rowid` —
holding the R-tree's pages as opaque blobs. `sqlite_master` calls them
`table`; `PRAGMA table_list` calls them `shadow`, and the engine now
reads the latter and skips them. They are an index's storage, and the
engine already drops indexes for the reason its documentation gives: a
secondary index costs space in every commit and buys a backup nothing.
The first version of this provider mirrored them; the mirrored rtree
plus its three shadow tables was the geo data stored twice, one copy of
it churning on every location edit.

A virtual table whose module the engine lacks (an FTS5 table, say) is
skipped with a warning and counted in `virtual_tables_skipped`. Its
shadow tables are skipped too — silently mirroring those as a fallback
would be exactly the quiet success AGENTS.md warns about.

## Reading a live library

Photos keeps `Photos.sqlite` in WAL mode and does not checkpoint it:
between two snapshots of the sample library the main file did not
change size while the `-wal` file went from 1.5 to 3.7 MB. Anything
that copies `Photos.sqlite` alone sees a stale library. The engine's
`VACUUM INTO` snapshot reads through the WAL correctly, and because the
daemons hold the file open whether or not Photos.app is running, the
snapshot path is the only path — `snapshot = false` reads a file that
is always mid-write.

## macOS permissions

`~/Pictures/Photos Library.photoslibrary` is a location macOS protects.
A process without access gets `Operation not permitted` on a plain `ls`
of the bundle — not a prompt, and not a message that mentions
permissions.

Measured (2026-09-11, written up in
[`docs/dev/wizard_file_pickers.md`](/docs/dev/wizard_file_pickers.md)):
choosing the bundle in a standard **file** open panel grants the app
access, the grant is recorded against the app rather than the process
that showed the panel, and child processes inherit it. So in the app
the picker is the way in, and the wizard's field is `picks: "file"` —
a folder chooser shows a `.photoslibrary` as a file and cannot select
it. Whether the grant survives quitting and relaunching the app is not
measured; Full Disk Access (System Settings → Privacy & Security) is
the durable fallback, and what a terminal needs to run `datalib-dag`
against the config directly.

## Timestamps

Core Data stores dates as REAL seconds since 2001-01-01 00:00:00 UTC
with no zone. The zone the photo was taken in is beside it in
`ZADDITIONALASSETATTRIBUTES.ZTIMEZONENAME` / `ZTIMEZONEOFFSET`. The
mirror stores all three as they are; combining them into [the repo's
timestamp form](/AGENTS.md#timestamp-convention) is render's job, the
same caveat `lightroom/INGEST.md` records for `captureTime`.

## Running it

As a DAG step — the `apple_photos` stanza in
[`docs/user/config_examples/all_sources.toml`](/docs/user/config_examples/all_sources.toml):

```toml
[[groups]]
id = "apple_photos"
type = "apple_photos"

[[steps]]
group = "apple_photos"
function = "ingest"
[steps.params.library]
path = "~/Pictures/Photos Library.photoslibrary"
```

`library.path` may name the bundle or the `Photos.sqlite` inside it.

Or standalone:

```sh
bazelisk build //datalib/backend/etl/providers/apple_photos:apple_photos_ingest
bazel-bin/datalib/backend/etl/providers/apple_photos/apple_photos_ingest \
  --library ~/Pictures/Photos\ Library.photoslibrary \
  --db ~/backups/photos.doltlite_db
```

Reading the backup is as for lightroom (`dolt_log`, `dolt_diff_<table>`,
`dolt_history_<table>`; the Bazel-built `doltlite` shell). The queries
that matter most:

```sql
-- Which photos were favourited or un-favourited in the latest run?
SELECT to_ZUUID, from_ZFAVORITE, to_ZFAVORITE FROM dolt_diff_ZASSET
 WHERE diff_type = 'modified' AND to_commit = 'HEAD'
   AND from_ZFAVORITE IS NOT to_ZFAVORITE;

-- Every album a photo has been in, ever.
SELECT h.commit_date, a.ZTITLE FROM dolt_history_Z_33ASSETS h
  JOIN ZGENERICALBUM a ON a.Z_PK = h.Z_33ALBUMS
 WHERE h.Z_3ASSETS = (SELECT Z_PK FROM ZASSET WHERE ZUUID = '…');
```

## What render will need

Everything `lightroom/INGEST.md` lists under the same heading, with the
paths already resolved: an asset is `originals/<ZDIRECTORY>/<ZFILENAME>`
under the bundle, and its edited version is under `resources/renders/`.
The cheapest way to the pixels is the one already in the tree: a
`media` source pointed at `<library>/originals`, which yields EXIF,
dimensions and `payload_blake3` per file, joinable to `ZASSET` on
`ZFILENAME`.

## Apple Music is not the same case

Issue #370 asked about Apple Music too. `Music Library.musiclibrary/
Library.musicdb` is not SQLite: it starts with `hfma`, the iTunes binary
library format. Only the sidecar `Extras.itdb` is SQLite, holding two
bookkeeping tables. The routes there are Apple's `iTunesLibrary.framework`,
the XML the app writes under File → Library → Export Library…, or the
`media` source over `Media.localized/Music/` — which already reads the
tags, and lacks only play counts, ratings, playlists and date-added.
