# gpx — download

Scans a folder for `.gpx` files and keeps each one as rows. Three things
are true of the store, and the tests in `tests/gpx_e2e.rs` hold it to
each:

- **Every file can be written back from its rows**, byte for byte for
  every writer met so far (`ingest::db::rebuild`).
- **A point two files hold is one row.** Track points, route points and
  waypoints are keyed by their content, so a copy of a file, or a
  week's export that overlaps a day's, adds no point rows.
- **A small edit is a small diff.** Change one point's elevation and the
  commit holds one point row out, one in, and one member row pointed at
  the new one. Rename a file, even while retitling it, and the commit
  holds one `gpx_files` row out and one in.

The contracts every provider honors are in
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).
It renders nothing yet: query the store directly (§"Reading the store").

## Tables

A GPX file is a `<gpx>` root holding `wpt`s (waypoints), `rte`s (routes,
each a list of `rtept`s) and `trk`s (tracks, each a list of `trkseg`
segments, each a list of `trkpt`s). All three kinds of point are GPX's
`wptType`.

| table | key | holds |
|---|---|---|
| `gpx_wpts`, `gpx_rtepts`, `gpx_trkpts` | `id`: the point's content key | one point; **shared by every file that holds it** |
| `gpx_files` | `path`, relative to the scanned folder | the file's hash, `file_key`, and everything outside its points' lists |
| `gpx_file_wpts` | `(file_key, ord)` | which waypoints the file lists, in order |
| `gpx_rtes`, `gpx_rte_rtepts` | `(file_key, rte)`, `(…, ord)` | each route, and its points in order |
| `gpx_trks`, `gpx_trksegs`, `gpx_trkseg_trkpts` | `(file_key, trk)`, `(…, seg)`, `(…, ord)` | each track, its segments, and each segment's points in order |

`file_key` is 16 hex digits minted when a file is first stored, and kept
by the file from then on, across renames (§"Renames"). It is short
because the member tables repeat it on every row, and every per-file
row leads with it, so all of one file's rows sort together.

**Which rows came from which file** is the member tables: a point
belongs to every file whose member rows name its id. Every per-file row
is under its file's `file_key`, so a file's rows are a range.

### A point row

`lat`, `lon`, `ele` and `time` are columns, as text exactly as written
(`1895` and `1895.0` stay different). `time_ms` is `time` in unix
milliseconds, for sorting and range queries; a time with no zone is read
as UTC, which is what GPX says its times are. Everything else the point
had — `name`, `desc`, `sym`, links, `<extensions>` — is `rest_xml`, its
children written compactly. `ele` and `time` are lifted into columns
only when they are the point's first children and plain text, as the
schema orders them; otherwise they stay in `rest_xml` and the columns
are NULL. `attrs_xml` holds the attributes as written when they are
anything but `lat`, `lon` in that order.

No other field is copied out into a column. A later pass (H3 cells, the
grid) reads what it wants from `rest_xml`.

### The point key

A v8 UUID: the leading 48 bits are `time_ms` (zero for a point with no
time), the rest a blake3 hash of every column but `time_ms`
(`datalib_id::stamped_hash`). Two consequences:

- The key is the content, so a stored point never changes; an edit is a
  new row, and the old one goes when no file names it.
- Keys sort by time, so one track's points sit together in the tree and
  a scan's writes touch few pages
  ([what a write costs](/datalib/backend/etl/README.md#what-a-write-costs-the-transaction-is-the-unit-and-the-key-decides-the-size)).

Whitespace between a point's children is not part of it (§"Layout"), so
the same point indented differently by two writers is one row.

### `ord`: order that survives an edit

A member row's `ord` is part of its key, so it is chosen so that the
edits people make move nothing else. When every point in a list has a
time and the times strictly increase — every recorded track met so far
— `ord` is the time in milliseconds (`point_order = 'time'`): deleting a
stray point deletes its row and leaves the rest alone. Otherwise `ord`
is the position in the list (`point_order = 'position'`), and an insert
or a delete renumbers what follows. Routes, which have no times, are the
usual case.

## Layout and fidelity

Everything up to the end of the `<gpx …>` start tag (`prolog`) and from
`</gpx>` on (`epilog`) is kept as written, so the XML declaration, the
namespace declarations and a writer's habit of one attribute per line
come back untouched.

Between the two, the whitespace a writer put between elements is not
stored with each element. It is `gpx_files.layout`: one string per
element path (`gpx/trk/trkseg/trkpt`) for the whitespace before a start
tag and one for before an end tag, plus how a self-closing tag ends
(`/>` or ` />`). Keyed by path rather than depth because writers vary
by element: My Tracks writes a track's `<extensions>` on one line and
its points one per line, at the same depth.

Ingest writes every file back from its rows and compares; the result is
`gpx_files.fidelity`:

| value | meaning |
|---|---|
| `exact` | byte for byte |
| `equivalent` | the same elements, attributes and text; whitespace between elements, attribute order or quoting, or comments differ |
| `lossy` | something moved or went missing |

Comments are dropped, which is all `equivalent` costs that matters. A
`lossy` file is reported as a warning in the store's `problems`
(`lossy:gpx_files:<path>`, rule `gpx_round_trip`), every run until it
changes. The one way known to produce one is a file that interleaves
its `wpt`s, `rte`s and `trk`s or puts other elements between them: the
rows keep each kind's order but not the interleaving, and the file comes
back in schema order.

Measured on seven real files from four writers (Geo Tracker, Maprika,
Maprika for Android, My Tracks): all `exact`.

## Renames

A file keeps its `file_key` when it is renamed, so its tracks, segments
and member rows stay where they are and only its `gpx_files` row moves.
A file read at a path the store does not hold takes, in order:

1. the key of the path `fsscan` saw it move from, when the bytes are
   unchanged;
2. else the key of a file gone from the tree this scan that shares at
   least half the points of the larger of the two — the one sharing the
   most, ties to the first by path. This is what catches a rename made
   while editing the file's metadata, whose bytes differ;
3. else a new key: a hash of the path and the content, stepped past any
   key a stored file holds, so a new file at a path a renamed file left
   does not collide with it.

Either way the file's rows are then diffed against what its key holds,
so a guess that is wrong costs rows, never correctness: the store ends
up holding exactly what the file says. Nothing is matched when the walk
reported errors, since a folder that failed to list looks like files
that vanished. A copy is never a rename (the original is still there),
so it gets its own key and its own member rows.

The key therefore depends on the folder's history as well as its
contents: a store that watched a file being renamed keeps the key the
old path was given, where a store built fresh from today's folder mints
one from the new path. The rows under the key are the same either way.

## What a scan does

`fsscan` walks the folder and hashes only what the host's fingerprint
cache cannot vouch for; the cursor in `ingested_files` (scope
`gpx/files`) says which files this source already has. An unchanged
file costs a `stat` and writes nothing. For each file that is new,
changed or moved:

1. Parse it, split it into rows, write it back and measure fidelity.
2. Pick its `file_key` (§"Renames").
3. Insert the point rows not already stored.
4. Diff the file's per-file rows against what the store holds under its
   key, and write only what differs.
5. Stamp the cursor in the same transaction, and forget the old path of
   a file it took the key of.

Files are written in transactions of about 50,000 points. A file that
is gone has its per-file rows deleted. Last, every point some file
stopped naming this run is deleted if no member row names it any more
— one pass over the member table, since there is no index on the point
id (one would be as large as the table).

A file that will not parse — not UTF-8, not well-formed, or a root
other than `<gpx>` — is a `record:gpx_files:<path>` error in
`problems`, is not stamped, and is tried again next run. The rest of
the folder is ingested.

## Known gaps

- **Nothing renders.** No markdown, no grid rows, no map.
- **GPX only.** KML (including Google's `gx:Track`, a list of `when`s
  beside a list of `coord`s) would map onto the same point tables.
- **Cloud placeholders are read.** Unlike `media`, a Dropbox online-only
  `.gpx` is fetched to be hashed; GPX files are small.
- **Prefixed GPX elements** (`<gpx:trk>`) are not recognised as tracks:
  the whole file lands in `gpx_files.head_xml`, intact but with no point
  rows.

## Reading the store

Stock `sqlite3` cannot open a `.doltlite_db`; use the doltlite shell
(`bazelisk build //third-party/doltlite:doltlite`), or turn the store
into a plain SQLite file:

```sh
datalib-doltlite -readonly <root>/gpx/ingest/entities.doltlite_db .dump | sqlite3 gpx.sqlite
```

```sql
-- Every file, how many track points it has, and how well it comes back.
SELECT f.path, f.fidelity, count(m.trkpt_id) AS trkpts
  FROM gpx_files f LEFT JOIN gpx_trkseg_trkpts m USING (file_key)
 GROUP BY f.path ORDER BY f.path;

-- One file's track, in order.
SELECT p.time, p.lat, p.lon, p.ele
  FROM gpx_files f
  JOIN gpx_trkseg_trkpts m USING (file_key)
  JOIN gpx_trkpts p ON p.id = m.trkpt_id
 WHERE f.path = 'hikes/2026-01-01.gpx'
 ORDER BY m.trk, m.seg, m.ord;

-- Files that share points, and how many.
SELECT a.path, b.path, count(*) AS shared
  FROM gpx_trkseg_trkpts x
  JOIN gpx_trkseg_trkpts y ON y.trkpt_id = x.trkpt_id AND y.file_key > x.file_key
  JOIN gpx_files a ON a.file_key = x.file_key
  JOIN gpx_files b ON b.file_key = y.file_key
 GROUP BY a.path, b.path ORDER BY shared DESC;

-- What the last scan changed in the points.
SELECT diff_type, count(*) FROM dolt_diff_gpx_trkpts('HEAD^1', 'HEAD') GROUP BY diff_type;
```

## Fixtures

`tests/fixtures/gpx_tng/` holds five made-up files, one per writer style
met so far; its README says which style each one copies.
