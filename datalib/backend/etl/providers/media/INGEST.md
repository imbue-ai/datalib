# media — download

Scans a local directory tree for audio, images, video and M3U
playlists, records what each file is, hoists the metadata worth
querying into typed columns, and computes a second hash over the part
of each file that is not metadata.

Row shapes are in [`src/ingest/schema_raw.rs`](src/ingest/schema_raw.rs);
the contracts every provider honors are in
[`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md).

## Relationship to `fsindex` and `pdf`

All three scan a local tree. `media` and `pdf` walk it through
[`datalib_etl::fsscan`](/datalib/backend/etl/src/fsscan.rs), which
hashes only what the host-wide fingerprint cache cannot vouch for
(`etl/README.md` §"Answering "did it change?" for a file-backed
source"); `fsindex` has its own walker over the same blake3 and
Unison-cursor primitives in
[`datalib_etl::fswalk`](/datalib/backend/etl/src/fswalk.rs).

They are separate **sources** because they answer different questions:

| | `fsindex` | `pdf` | `media` |
|---|---|---|---|
| Question | "what is in this tree?" | "what documents do I have?" | "what music, photos and video do I have?" |
| Scale | 10⁷ entries | 10³ documents | 10⁵–10⁶ files, terabytes |
| Keyed on | path | content hash | content hash |
| Second identity | — | `content_blake3` | `payload_blake3` + scheme |
| Per-item cost | one `stat`, sometimes one `read` | a parse and a conversion | a sniff, a container walk, a second hash |
| Render side | none | markdown + `grid_rows` | **none** |

## No render side

`media` fills its raw store and stops, the way `fsindex` does.
"Download-only" is structural — `processor::plan_render` returns no
processors — not a flag.

There is no text to render. A small metadata card per item would let
the qmd index answer "photos from the Yosemite trip", but it is not
built, for two reasons: the qmd index's cost at 10⁵–10⁶ items is
unmeasured, and a grid of text rows is the wrong surface for this data,
which wants thumbnails, a map, a time scrubber and an album view.

Nothing here reaches `grid_rows`. Query the store directly; see
§"Inspecting a scan".

## The payload hash

`media_items.payload_blake3` is a digest over the part of a file that
carries the signal — the MPEG frames, the `data` chunk, the
entropy-coded scan, the sensor strips — with tags, EXIF, XMP, ICC
profiles, embedded previews and container padding left out.

It exists because in a personal library, *metadata edits are most of
what ever happens to a file*. Retag an MP3 and the ID3v2 block at the
front is rewritten. Add a keyword to a JPEG and `APP1` is rewritten,
sometimes along with the embedded thumbnail. Move a slider in Lightroom
and the DNG's preview IFD is re-rendered. In every case `blake3(bytes)`
— the right primary key — moves, and the recording did not.

| Container | Scheme | What is hashed |
|---|---|---|
| WAV | `wav.data.v1` | the `data` chunk |
| AVI | `avi.movi.v1` | the `movi` list |
| MP3 | `mp3.frames.v1` | MPEG frames, minus ID3v1/ID3v2/APEv2 **and the Xing/Info/VBRI frame** |
| FLAC | `flac.frames.v1` | audio frames, past every metadata block |
| JPEG | `jpeg.scan.v1` | `SOF`/`DQT`/`DHT`/`DRI`/`SOS` + entropy data; no `APPn`, no `COM`, nothing past `EOI` |
| PNG | `png.idat.v1` | critical chunks + `tRNS` |
| TIFF/DNG | `tiff.strips.v1` | strips and tiles of the full-resolution IFDs |
| MP4/M4A/MOV | `bmff.samples.v1` | per-track sample bytes, from the sample tables |
| HEIC/HEIF | `bmff.items.v1` | item extents, except the `Exif` and `mime` items |

Everything else — GIF, WebP, AIFF, Ogg, Matroska, and anything we
could not parse — is recorded with a NULL payload hash. Adding a recipe
later is a pure addition: the work list is already a query
(`SELECT … WHERE payload_blake3 IS NULL`).

### It is a hint, never a key

Re-encode a JPEG at the same quality, run an MP3 through a different
LAME build, let `optipng` recompress an `IDAT`, and the payload hash
moves although nothing you can see or hear changed. It **splits where it
ideally would have merged** — the direction `pdf`'s `content_blake3`
takes, for the same reason — so the primary key stays `blake3(bytes)`.

Three rules follow, all load-bearing:

1. **An unparsable container gets NULL, not the file hash.** A fallback
   would make the column claim a metadata-independence the format never
   gave it, and every downstream `GROUP BY payload_blake3` would
   silently believe it.
2. **`payload_scheme` is stored beside the digest.** Two payload hashes
   are only comparable under one recipe, so changing what a recipe
   excludes means bumping its version — which makes the mismatch
   visible instead of turning a fixed bug into a silent false merge.
3. **It is not a perceptual hash.** That answers a different question
   with the opposite failure mode. If we want one it gets its own
   column and its own name.

### Recipe notes worth knowing

- **The Xing frame is the subtle one.** It is a structurally valid MPEG
  frame that decodes to silence and carries the VBR seek table, the
  gapless delay/padding, and the LAME ReplayGain fields. The seek table
  is expressed in byte offsets and a total file size, so *any tag edit
  that changes the file's length invalidates it* and tools rewrite it —
  add cover art and this frame moves although the audio did not. A
  "skip the tags" implementation that left it in would defeat the column
  for a large part of a real library.

  It does not cover `mp3gain` *applying* gain, which is worth being
  clear about because the name suggests otherwise: mp3gain rewrites the
  `global_gain` field in every frame's side information — a volume
  change with no re-encode — so it moves `mp3.frames.v1` as surely as a
  re-encode would. Only its undo bookkeeping lands in tags and the Xing
  frame.
- **BMFF hashes sample bytes, not the `mdat` box.** `mdat`'s layout —
  chunk interleave, padding, where in the file it sits — is a muxer
  decision that a `faststart` rewrite can change without touching a
  coded sample, so the sample tables are walked instead and only the
  samples are hashed, grouped per track and ordered by track id.

  **Changing any track changes the file's payload hash**, which is the
  behavior you want: a clip with different pictures is a different clip.
  The grouping is not a way to make a video edit invisible; what it buys
  is that the digest is a function of the tracks' contents and nothing
  else, so reordering `trak` boxes or inserting padding leaves it alone.
  Per-track digests are computed and discarded — storing them (a
  `media_streams` table keyed on `(item, track_id)`) is what would make
  "which files share this audio track?" a real query, and it is not
  built. Samples within a chunk are contiguous, so the plan is one range
  per chunk rather than one per sample: a two-hour film is a few
  thousand ranges.
- **DNG previews are excluded via `NewSubfileType`.** This is the single
  biggest win for a Lightroom-shaped library. If no IFD qualifies, every
  image-bearing IFD is used instead — hashing the strange thing beats
  returning NULL.
- **JPEG's ICC profile is excluded**, which is the one genuinely
  arguable call: it is metadata by structure but it changes rendering,
  so a file re-tagged sRGB → Display P3 keeps its payload hash. See
  [`src/ingest/payload/jpeg.rs`](src/ingest/payload/jpeg.rs) for
  the argument.
- **FLAC's `STREAMINFO` MD5 is deliberately unused.** It is a hash of
  the *decoded* samples and would survive re-encoding, which
  `flac.frames.v1` does not — but putting a different algorithm over a
  different input into the same column would quietly invalidate
  `GROUP BY payload_blake3`. Decoded-sample identity belongs in its own
  column; that is a real follow-up.

Single-group plans hash their bytes directly, so a WAV's
`payload_blake3` is exactly `b3sum` of its extracted `data` chunk and
can be checked by hand. Multi-group plans (BMFF tracks, TIFF image
IFDs) hash each group and then digest the concatenated group digests.

## Two class tables, not three

The obvious split is music / photos / video. The one that falls out of
the data is **audio versus visual**, because the line that matters is
the kind of metadata, not the medium:

- `media_audio` holds *tags describing a recording* — artist, album,
  track number. Typed by a person or a database; they describe the work.
- `media_visual` holds *EXIF describing a capture* — camera, lens,
  exposure, GPS, the moment the shutter opened. Written by a device.

Video sits squarely on the EXIF side. A phone's `.mov` carries the same
make, model, capture time and coordinates as the `.heic` beside it, and
a Live Photo is literally the two together. A third table would mean
duplicating a dozen capture columns and then joining them back for every
"what did I shoot on this trip?" query. The genuinely video-only fields
(`frame_rate`, the two codec columns) are nullable columns on the shared
table. Duration belongs to both, so it lives on `media_items`.

## Both kinds of metadata, where the container carries both

An item has one `media_class`, but a file need not. An MP4 music video
carries `ilst` tags *and* capture metadata; an `.m4a` recorded on a
phone carries a date in `©day`. So the readers are chosen by what the
**container** can hold — `Container::may_have_tags` and
`may_have_exif`, and BMFF is in both — rather than by the item's class,
which would silently drop whichever half did not match.

A class-table row is written only when its reader found something, so
this does not put an all-NULL `media_visual` row behind every MP3.
Properties alone (bitrate, sample rate) do not count as "found" for a
non-audio file, or every video would get a `media_audio` row for the
sound track inside it and bury the music.

Choosing by container is also what keeps the cost down: a photo library
does not open every JPEG to ask a tag reader a question whose answer is
always no.

## Extension decides what to visit; bytes decide what it is

The walk predicate is extension-only, because it runs once per entry in
a tree that may hold millions and the alternative is opening every file.
Everything downstream reads the leading bytes, because extensions lie in
ways that matter here: `.m4a`, `.mov`, `.mp4` and `.heic` are all ISO
base media files distinguished by their `ftyp` brand; `.dng` is a TIFF;
and a `.jpg` holding PNG bytes is common enough in exported libraries to
be worth not mis-parsing.

A file whose bytes match nothing we know is recorded as
`container = 'unknown'` — a true statement — rather than being labelled
from its name.

## Playlists

`.m3u` / `.m3u8`, into `media_playlists` + `media_playlist_entries`.

**The raw target string is the data.** In a library of any age most
entries are broken: Windows separators, a drive that no longer exists,
a path relative to a directory the playlist was later moved out of, a
song deleted years ago. Storing only what resolves would throw away the
most interesting rows — "this playlist references 240 tracks and I still
have 187" is a question worth asking, and the 53 missing ones are the
answer. So `target_raw` is verbatim, nothing is dropped for failing to
resolve, and nothing is deduplicated or reordered: `position` is the
entire content of the format.

**Resolution stops at the path.** `resolved_path` is computed from the
raw target and the playlist's own location — pure string work, no I/O.
Whether a *file* is there is deliberately not a column:

```sql
SELECT e.position, e.target_raw, f.blake3
  FROM media_playlist_entries e
  LEFT JOIN media_files f ON f.id = e.resolved_path
 WHERE e.playlist_id = ? ORDER BY e.position;
```

A stored resolved hash or count would be a cached join result: it
goes stale the moment a track is added or removed without the playlist
being rescanned, so a playlist would keep claiming a song was missing
after you restored it. The join is always current. The step still reports `entries_in_tree=`, which is a fact
about the playlist *text* (how many entries name somewhere inside the
tree at all) rather than about the disk.

Resolution is textual, never `canonicalize`: the interesting case is a
target that *does not exist*, which a filesystem call cannot resolve,
and following symlinks would let a playlist point outside the tree.

### HLS manifests are not playlists

`.m3u8` is also the extension for HTTP Live Streaming manifests, which
browser and app caches write by the thousand. Extension cannot separate
them; the contents can. Any file carrying an `#EXT-X-` tag is counted
into `hls_skipped=` and not recorded. That test is the format's own, not
a guess about filenames.

### Encoding

`.m3u8` means "M3U, UTF-8" — the `8` *is* the encoding. Plain `.m3u`
predates that and is usually in the writer's local codepage. We decode
UTF-8 when the bytes are valid UTF-8 and fall back to Latin-1 when they
are not, which cannot fail: every byte maps to a code point, so the
worst case is mojibake in one field rather than a lost playlist.

## Cloud placeholders are skipped, loudly

Reading a Dropbox "online-only" file, a macOS dataless file or a
OneDrive stub is not a cheap mistake — it asks the sync client to
materialize the file, so a first scan of an evicted library would try to
pull the whole thing down.

Detection is `blocks == 0 && size > 0`. That is a heuristic with two
known false-positive paths: a filesystem that reports no block counts at
all looks entirely evicted, and an APFS file stored inline via `decmpfs`
can report zero blocks while its bytes are right there. Neither is
likely for a media tree — macOS does not compress user files by default,
and an already-compressed JPEG or MP3 is the worst possible candidate
for it — but both are real.

So the failure is made loud rather than silent: every skip is counted
into the step's `dataless_skipped=` summary and logged at `info`, so a
corpus that comes back empty says why in the step's own output.
`skip_dataless = false` turns it off. The precise test would be macOS's
`SF_DATALESS` stat flag, which `std`'s `MetadataExt` does not expose;
reaching it means a `libc` dependency, which has not seemed worth it.

A file skipped this way keeps the rows an earlier scan wrote for it:
declining to read a file is not finding it gone. Pinned by
`a_dataless_file_keeps_its_row`, which makes the placeholder as a sparse
file and passes without checking anything on a filesystem that gives
one blocks.

iCloud's eviction markers need no handling: they are named
`.track.mp3.icloud`, so the extension filter never visits them.

## Capture times are stored as the file wrote them

A stamp that belongs to the record stays as the source wrote it
(AGENTS.md §"Timestamp convention"). EXIF's `DateTimeOriginal` has no
offset — it is local wall-clock with no zone, and the offset only
arrived with EXIF 2.31's `OffsetTimeOriginal`, which most cameras still
omit.

So `media_visual.captured_at` carries an offset **when the file supplies
one** and is naive (`2364-04-13T08:45:00`) when it does not. The
alternatives were worse: `+00:00` would assert the photo was taken in
UTC, and the scanning machine's offset would assert it was taken
wherever the scan ran. A missing offset is a fact about the file, and
the naive form is the only encoding that states it.

## A DNG's dimensions are the photograph's

EXIF is read from the primary IFD, and a DNG's primary IFD is usually
the embedded *preview* — so an EXIF-only reader reports the preview's
size, wrong by a factor of five or more.

`media_visual.width`/`height` therefore come from the same walk the
payload hash uses: the first IFD that carries image data and is not
flagged reduced-resolution. `DefaultCropSize` wins when present, because
a sensor reads out slightly larger than the visible frame and the crop
size is what every RAW tool — and the photographer — calls the image
size. Pinned by `dimensions_come_from_the_container_when_there_is_no_exif`
in `tests/media_e2e.rs`, whose fixture has a 64×48 preview in front of a
320×240 sensor image with a 316×236 crop.

For video, the QuickTime `©day` tag wins over `mvhd`'s creation time,
because it carries a real offset where `mvhd` claims a UTC that many
cameras do not actually write. (`mvhd` version 0 also stores the time as
32-bit seconds since 1904, which runs out in 2040 — the fixture corpus
is set in 2364 and so uses version 1.)

A GPS-carrying photo could have its true offset recovered by
differencing `DateTimeOriginal` against the UTC `GPSDateStamp` /
`GPSTimeStamp`. That is a genuine follow-up, not a rejection.

## What a rescan skips, and what it does not

The fingerprint cache short-circuits **files**, not directories. Every
scan walks the whole tree and `stat`s every entry it would index; what it
skips for a file whose stat is unchanged is the `read`. Content already
in `media_items` is not identified again, so the container parse, the
payload hash and the metadata extraction — all of the expensive work —
run once per distinct content. A settled tree reports `hashed=0`.

There is no directory-level short-circuit; that is `fsindex`'s Merkle
tree, and a document provider hashes leaves and stops. So a million-file
library costs a million `stat` calls per scan — cheap (a few seconds)
but not zero.

`a_rescan_after_edits_changes_exactly_what_it_should` in
`tests/media_e2e.rs` pins the accounting: six kinds of edit — a retag, a
touch-without-edit, a duplicate copy, a new file, a deletion, and a
shortened playlist — followed by a rescan that hashes exactly five files
and identifies exactly two new items.

## What the reconcile does not delete

A path the scan did not read is not a path that is gone. A cloud
placeholder (above) and a file over `max_bytes` are found by the walk
and not read; both are in the scan's `present_unread`, and their rows
stay. A walk that reported an error deletes nothing at all, because an
unreadable folder's files look exactly like deleted ones; the error is
the source's `listing:files` problem. `a_path_the_scan_passed_over_keeps_its_row`
in `tests/media_e2e.rs` covers all three, the placeholder as a sparse
file (a size and no blocks).

## Interrupting a scan

The path-keyed tables are reconciled at the **end** of a scan, not
truncated at the start as `pdf` does. The scan loads the stored paths
first; each visited path is taken out of that set, and whatever remains
at the end is exactly the set of paths that disappeared. Those rows are
deleted by id, and the count lands in the step's `removed=` summary.

Two things follow:

- **A killed scan deletes nothing.** The path table keeps every row it
  had, plus the batches the scan flushed (`BATCH_SIZE` rows each), and
  the next scan reads no bytes for unchanged files. Pinned by
  `a_failed_scan_leaves_the_rescan_cursors_intact` in
  `tests/media_e2e.rs`, which induces the failure with a malformed
  ignore glob.
- **Rows for since-deleted files linger** after an interrupted run,
  until a scan completes. A row that outlives its file is corrected by
  the next full scan.

## When part of a scan fails

The scan goes on, and what it could not do is a `problems` row:

- **An entry the walk could not read** (a folder it may not list, a
  file that would not hash, a dangling link) means a path the walk did
  not see may only be one it could not see, so **that scan deletes
  nothing** and leaves a `listing:files` row. The next clean walk
  deletes what is really gone and clears the row
  (`a_walk_with_errors_deletes_nothing`).
- **A media file that would not open or parse** is
  `record:media_files:<path>`, and **a playlist that would not read**
  `record:media_playlists:<path>`; either keeps the rows an earlier scan
  wrote. An item that never identified is not in `media_items`, so every
  scan tries it again, and the row goes with the first that succeeds.

Both sets are replaced whole each scan, which is right because each
scan walks the whole tree and retries everything it could not read —
except a file it did not try: one under an entry its walk could not
read, or declined as a cloud placeholder, keeps its row.

Reconciliation is a **set difference, not a timestamp sweep**. The
simpler `DELETE … WHERE last_seen_at <> <this run>` looks equivalent and
is not: `DATALIB_DAG_NOW` is pinned per run, so two runs sharing a
pinned `now` — a retry, a test — would sweep nothing and silently keep
rows for deleted files. Set difference has no clock in it.
`deletions_are_reconciled_without_a_clock` runs both scans under one
`NOW` for exactly this reason.

What is *not* interrupt-tolerant is the doltlite commit, and that turns
out not to matter. Batches are written as SQLite transactions against
the working set, which lives in the file and persists without a
`dolt_commit`
([doltlite.md § Branches](/docs/dev/doltlite.md#branches-head-and-the-working-set));
the commit at the end of the step is a history marker, not what makes
the work durable. The expensive per-item work — container
parse, payload hash, metadata read — is keyed on content and lives in
`media_items`, which is never deleted, so an interrupted run's parses
survive too.

## Orphaned items

`media_files`, `media_playlists` and `media_playlist_entries` are
reconciled every scan (see above), so a deleted file disappears on its
own. `media_items`, `media_audio` and `media_visual` are **not** — they
are keyed on content, which has no notion of "no longer present", and
dropping them would lose when the item was first seen
(`media_items_bookkeeping.fetched_at_utc`) and force a re-parse of every
item whose path merely moved.

So deleting the last copy of an item leaves an unreferenced
`media_items` row, deliberately: the row is cheap and it preserves the
record that the item was once here. Reaping them is a
`DELETE … WHERE blake3 NOT IN (SELECT blake3 FROM media_files)` whenever
we want it; the rows stay in earlier commits, but HEAD stops recording
that the item was once here. Pinned by
`a_deleted_file_disappears_from_the_path_table_but_the_item_remains` in
`tests/media_e2e.rs`.

## Known gaps

- **No render side**, so nothing appears in the grid. See above.
- **No Matroska, Ogg, GIF or WebP payload recipe.** Recognized and
  recorded; `payload_blake3` is NULL.
- **No video frame-accurate duration for AVI** beyond
  `total_frames × µs_per_frame` from `avih`.
- **No keyword extraction, and no column pretending otherwise.**
  Keywords live in IPTC or XMP, not in the EXIF IFD, so
  `kamadak-exif` cannot reach them — it does not even expose the
  Windows `XPKeywords` tag. A column that is structurally always NULL
  would read as "this library has no keywords". Getting them needs an
  XMP reader.
- **Nothing in CI runs the step.** The e2e suite calls `ingest::fetch`
  directly, so `processor.rs` and the `RawStoreSession` commit around it
  are covered only by hand-running `datalib-dag`; the `ingested_tng`
  fixture pipeline runs only sources that render.
  `download_only_sources_plan_a_download_and_no_render` in
  `datalib_step/src/dispatch.rs` covers the config and planning half.
- **`skip_dataless` is exercised only where a sparse file has no
  blocks** (APFS does; a filesystem that allocates them leaves
  `a_dataless_file_keeps_its_row` checking nothing).
- **A scan interrupted before its first flush loses that batch**, up to
  `BATCH_SIZE` files. Everything already flushed survives.

## Inspecting a scan

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
db=<root>/media/ingest/entities.doltlite_db

# What is in the library?
$dl -readonly $db "SELECT media_class, container, COUNT(*) FROM media_items
         GROUP BY media_class, container ORDER BY 3 DESC;"

# One recording, many files: every item that shares a payload hash.
# This is the query the column exists for.
$dl -readonly $db "SELECT payload_blake3, COUNT(*) c, GROUP_CONCAT(blake3)
           FROM media_items WHERE payload_blake3 IS NOT NULL
          GROUP BY payload_blake3 HAVING c > 1;"

# …and the human-readable form for music.
$dl -readonly $db "SELECT i.payload_blake3, GROUP_CONCAT(f.id)
           FROM media_items i JOIN media_files f ON f.blake3 = i.blake3
          WHERE i.payload_blake3 IN (
                SELECT payload_blake3 FROM media_items
                 WHERE payload_blake3 IS NOT NULL
                 GROUP BY payload_blake3 HAVING COUNT(*) > 1)
          GROUP BY i.payload_blake3;"

# How much of the library has no payload recipe yet?
$dl -readonly $db "SELECT container, COUNT(*) FROM media_items
          WHERE payload_blake3 IS NULL GROUP BY container;"

# Duplicates by bytes: one item, many locations.
$dl -readonly $db "SELECT blake3, COUNT(*) c, GROUP_CONCAT(id) FROM media_files
         GROUP BY blake3 HAVING c > 1;"

# Playlists, and how much of each one survives. A join, not a stored
# count -- so it is right even if the tree changed since the scan.
$dl -readonly $db "SELECT p.id, p.title, p.entry_count,
                SUM(f.blake3 IS NOT NULL) AS have,
                p.entry_count - SUM(f.blake3 IS NOT NULL) AS missing
           FROM media_playlists p
           JOIN media_playlist_entries e ON e.playlist_id = p.id
           LEFT JOIN media_files f ON f.id = e.resolved_path
          GROUP BY p.id ORDER BY missing DESC;"

# The music a playlist remembers and the disk does not.
$dl -readonly $db "SELECT e.playlist_id, e.position, e.target_raw
           FROM media_playlist_entries e
           LEFT JOIN media_files f ON f.id = e.resolved_path
          WHERE f.blake3 IS NULL AND e.target_kind = 'relative'
          ORDER BY e.playlist_id, e.position;"

# What was shot where.
$dl -readonly $db "SELECT f.id, v.captured_at, v.camera_model, v.gps_lat, v.gps_lon
           FROM media_visual v JOIN media_files f ON f.blake3 = v.blake3
          WHERE v.gps_lat IS NOT NULL ORDER BY v.captured_at;"

# What changed since the last scan.
$dl -readonly $db "SELECT diff_type, from_id, to_id
           FROM dolt_diff_media_files('HEAD^1', 'HEAD');"
```

## Fixtures

`tests/fixtures/media_tng/` is generated by
[`//tests/fixtures/make_media_fixtures.py`](/tests/fixtures/make_media_fixtures.py)
— hand-built bytes rather than encoder output, for the reasons
[`../pdf/INGEST.md`](../pdf/INGEST.md) §"Fixtures" gives for its own
generator. Regenerate with:

```sh
uv run python tests/fixtures/make_media_fixtures.py
```

The corpus is built around **metadata-only variants** — six pairs whose
signal is identical and whose metadata is not — because those are what
prove the payload hash does its job. It also carries two BMFF files that
hold *both* kinds of metadata: `holodeck_clip.mp4` (a music video with
`ilst` tags and a capture date) and `bridge_recital.m4a` (an audio file
with a recording date), which are what caught the class-chosen reader
dropping half of each.

`lofty` refuses a BMFF file outright — "failed to parse Mp4 file", no
further detail — if its `stsd` sample entry is a stub rather than a
real 28-byte `AudioSampleEntry`, or if `mdia` has no `hdlr` atom naming
the track `soun`. Our own parser needs neither, so a fixture has to
satisfy `lofty` too.

The corpus also carries a byte-identical copy in a second folder, an untagged file that must land on the same
payload hash as its tagged sibling, a truncated MP3, a file whose
extension we index and whose bytes we cannot parse, an HLS manifest
wearing the `.m3u8` extension, a Latin-1 playlist, and a `readme.txt`
that must never appear in any table.

Those assertions have been checked in the failing direction, not only
the passing one: breaking `mp3.frames.v1` to hash the whole file, and
`tiff.strips.v1` to stop excluding reduced-resolution IFDs, fails
exactly `retagging_an_mp3_leaves_the_payload_hash_alone` and
`re_rendering_a_dng_preview_leaves_the_sensor_data_identity_intact` and
nothing else.
