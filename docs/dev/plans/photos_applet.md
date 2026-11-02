# Photos: a row per picture, and an applet that shows it

*Proposal (2026-11-02), partly built: §8 says which steps landed and
where the build departed from the plan. Lightroom Classic first;
written so Apple Photos can follow through the same shape (§3).*

Before this plan, a Lightroom source was a versioned backup of the
catalog and nothing else: its `render_markdown` step wrote only the
storage report, so not one photo reached the grid or the search bar.
This plan gives every photo and video in a
catalog one row in the grid, findable by when it was taken, its caption,
its keywords, the people tagged in it, the albums it is in and where it
was taken, and adds a `photos` applet that draws the picture: a
thumbnail in a grid cell, a full-size view in a card, with the details
beside it.

Words used throughout:

- **Catalog**: Lightroom's `.lrcat`, a plain SQLite file. The `lightroom`
  ingest mirrors it table for table into a doltlite store, the
  **mirror**.
- **Preview**: a JPEG Lightroom rendered from the original *with the
  person's edits applied* (crop, exposure, black and white). Kept in the
  `<Catalog> Previews.lrdata` folder beside the catalog. A **smart
  preview** is a different thing: a small lossy DNG for editing offline,
  which a browser cannot show. This plan does not use smart previews.
- **Original**: the photo or video file the catalog points at, wherever
  it lives on disk.
- **H3 cell**: a hexagon on Uber's [H3](https://h3geo.org/) grid. Every
  point on Earth falls in one cell per **resolution**; resolution 4
  cells are about 1,800 km², resolution 10 about 0.015 km². A cell id is
  a 15-character hex string, so "photos taken in this hexagon" is an
  exact-match lookup.

## 1. What the example catalogs show

Read from `thadd3us/lightroom_db_diff` at `d8d1364` (catalog 03 with its
previews and `previews.db`). Everything in this section was
checked against those files; what was not is said.

**One photo is one `Adobe_images` row**, keyed by `id_global`, a UUID.
Everything else hangs off its `id_local`:

| What | Where |
| --- | --- |
| capture time | `Adobe_images.captureTime`: local wall-clock time with **no offset** (`2007-01-01T16:41:39`). The person can edit it, and did between catalogs 02 and 03. |
| the offset | only in the XMP packet, `Adobe_AdditionalMetadata.xmp` (zlib after a 4-byte length): `exif:DateTimeOriginal="2007-01-01T16:41:39-08:00"`. Where the camera wrote none, `xmp:CreateDate` sometimes has one (`+01:00` on photo 796). |
| title, headline | only in the XMP: `dc:title`, `photoshop:Headline`. |
| caption, copyright | `AgLibraryIPTC.caption`, `.copyright` |
| last edited | `Adobe_images.touchTime`, seconds since 2001-01-01 UTC, so an instant |
| GPS | `AgHarvestedExifMetadata.gpsLatitude`, `.gpsLongitude`, `.hasGPS` |
| place names | `AgHarvestedIptcMetadata` → `AgInternedIptcCity`, `…State`, `…Country`, `…Location`. Lightroom fills these itself from the GPS (`locationDataOrigination = 'gps'`). |
| camera, lens, exposure | `AgHarvestedExifMetadata` → `AgInternedExifCameraModel`, `…Lens` |
| rating, pick, color label | `Adobe_images.rating`, `.pick`, `.colorLabels` |
| keywords | `AgLibraryKeywordImage` → `AgLibraryKeyword` (a tree: `parent`, `genealogy`) |
| people | a keyword with `keywordType = 'person'`. Each face is an `AgLibraryFace` row with its rectangle in 0–1 image coordinates, named through `AgLibraryKeywordFace.tag` → that keyword. |
| albums | `AgLibraryCollectionImage` → `AgLibraryCollection`. Smart collections (`creationId = …smart_collection`) are saved searches with no member rows; the quick collection is `systemOnly = 1`. |
| the file | `rootFile` → `AgLibraryFile` → `AgLibraryFolder` → `AgLibraryRootFolder` (`absolutePath`, and `relativePathFromCatalog` for a library that moved with its catalog) |

**Previews are loose JPEGs in the new catalog, and one container file
each in the old ones.** Catalog 03 (database version 1504001) keeps one
file per size, `<a>/<abcd>/<file uuid>-<digest>_<long edge>`, e.g.
`…-3219a2e1…_320`, `_756`, `_1512`, `_3024`. The 2020 catalogs in the
same repo's history keep one `<file uuid>-<digest>.lrprev` holding every
size, each JPEG behind a short `AgHg` header naming its level. In both,
`<file uuid>` is `AgLibraryFile.id_global`, not the image's (true for all
seven photos).

**Which preview is current is said by `previews.db`**, beside the
previews. `ImageCacheEntry(imageId, uuid, digest)` gives one
`(uuid, digest)` per photo, `imageId` being `Adobe_images.id_local`;
`PyramidLevel(uuid, digest, level, longDimension, width, height,
fileSize)` lists that render's sizes. Catalog 03's folder holds up to
five digests for one photo (older renders Lightroom has not yet thrown
away), so the files alone cannot say which is the edited one. The tables
are the 2020 ones plus `fileSize`. All 19 sizes `ImageCacheEntry` names
current are on disk, each exactly its `fileSize`, and every file in the
folder has a `PyramidLevel` row.

**The current preview can be smaller than the original.** Lightroom
renders a size when something asks for it: photo 87's current render
goes up to 756 px of a 1200 px original, while an older render of the
same photo has a 1200 px level. `Pyramid.quality` (`thumbnail`,
`smallRender`, `standard`, `final`) says how far a render got.

Not in any example catalog: a video, a virtual copy, a RAW original, a
photo with no preview. Each needs a catalog that has one before the code
that handles it is trusted.

## 2. The shape of the change

```
lightroom/ingest ──► lightroom/render_markdown ──► unified_document_index/grid_index
   (mirror)            grid_rows + photos table        (the grid, search terms)
                       no .md files                          │
                                                             ▼
                    photos applet ◄── reads the render store's photos table
                    /photo/<uuid>, /photo/<uuid>/image ── reads Previews.lrdata
                                                        and originals, live
```

1. **Render writes a grid row per photo and no markdown.** It also writes
   a `photos` table into the same render store: one row per photo, in a
   shape that is not Lightroom's (§3).
2. **The applet reads that table and the picture files.** It never reads
   the mirror, so the applet knows nothing about Lightroom's schema, and
   an Apple Photos render that writes the same table is served with no
   change to it.
3. **The card is the applet's**, drawn from `/photo/<uuid>` when someone
   opens a photo row. There is no page on disk to open.

## 3. Render: one document per photo, with no file

### The rows

One `GridRow` per photo or video, its own document (`is_document`, one
row per document):

| column | value |
| --- | --- |
| `uuid` | `Identity::mint(Lightroom, source_id, None, "image", id_global, None)`. **No stamp**: the stamp has to be the row's `created_at`, and a capture time the person edits would then change the id ([`entity_ids.md`](../entity_ids.md)). |
| `kind`, `source_label` | `Photo` or `Video`; `Lightroom` |
| `created_at` | the capture time with its offset (§5) |
| `modified_at` | `touchTime`, the last edit in Lightroom |
| `touched_at` | the capture time, set explicitly, the way a calendar event's is its start: a newest-first grid of photos means newest *taken* |
| `conversation_name` | the title, else the headline, else the file name |
| `channel` | the folder, `pathFromRoot` |
| `author` | the IPTC creator, where there is one |
| body (→ `preview`, `content_hash`) | the caption |
| `upstream_id`, `upstream_entity_kind` | `id_global`, `image` |
| `item_count` | 1 |
| **`image`** (new) | `datalib:photo/<uuid>`: the pointer the grid draws a thumbnail from (§4) |
| **`latitude`, `longitude`** (new) | the GPS fix, where there is one |
| `supplied_terms` | below |

`image`, `latitude` and `longitude` are new `GridRow` columns, null on
every other source. `image` is a name in the
[datalib links](datalib_uris.md) sense, not an address: the row says
*what* picture, and which server hands out its bytes is decided when it
is drawn, so moving the bytes later moves no row. The same column can
later carry an email's image attachment. Latitude and longitude are
there for a map card, which a calendar event or a Garmin activity can
fill too.

### The search terms

The person asked for keywords, people, albums and place in the terms as
labels. Proposed:

| what | term kind | finds it |
| --- | --- | --- |
| each keyword, and each of its parents | `Label` | `label:sunset`; `label:Nobelpris` finds a photo tagged with a child keyword of it |
| each person tagged, by face or by person keyword | `Label` | `label:"Campino"` |
| each album | `Label` | `label:Landmarks` |
| city, state, country, location | `Place` (new) | `place:"San Francisco"` |
| H3 cells at resolutions 4, 6, 8, 10 | `Place` (new) | `place:8828308281fffff`, which a map card writes for you |
| the caption | `Caption` (new) | `caption:"golden gate"` |
| title, headline | `Title` | |
| `id_global` | `UpstreamId` | a UUID copied out of Lightroom |

Two new kinds and two new search keys, `place:` and `caption:`. People
go in as labels, as asked; whether `with:Campino` should also find them
is §9.

**The free-text box will not find photos.** Free text goes only to qmd,
and qmd indexes markdown files; a photo has none. `caption:` is the way
in. §9 lists the ways to change that.

### A document with no file

Planned as a change to `etl/render` (an optional `md_path`, `source_id`
from `markdowns`); built as none. The render store never writes a
document's file, its renderer does, so a renderer that writes none
leaves everything else as it is: the rows still name the `qmd_path` the
file would have, which keeps `source_id` and the id-from-path rule, and
qmd, whose collection is a glob over `.md` files, finds nothing. The
document carries one keyed section, a short text of the photo, so a
diff group can still compare two renders of it. `/chat/<uuid>` answers
a document whose file is missing and whose row has an `image` with that
name and an empty body.

The step stays `render_markdown`. The name lies a little for this
source, but renaming a step is a migration, and `grid_index` and the
wizard already know this one.

**Incremental**, as built: every photo is filed under one bucket,
declared over the whole tables render reads, so a sync where the catalog
did not change costs one lookup and one where it did renders every photo
again. Planned, and still to do for a large library: a bucket per photo
keyed by `id_global`. The rows that
change one photo are spread over a dozen tables, so the render diffs the
mirror between the commit it last rendered and HEAD on each, and
re-renders the union of photos they touch. A change to a shared row
(renaming a keyword or an album) re-renders every photo that uses it.

### The `photos` table: the applet's whole view of a source

One row per photo, written by render beside `grid_rows`, keyed by the
same `uuid`. Defined once, in a new shape crate
(`datalib/backend/photo_schema`, the way `contact_schema` defines
`NormalizedContact`), so Apple Photos fills the same table:

- what the card shows: title, caption, capture time and how its offset
  was known, camera, lens, exposure, rating, pick, color label,
  keywords as paths, people with their face rectangles, albums, place
  names, latitude, longitude, H3 cells, file name, folder, size in
  pixels, orientation, and for a video its length;
- where the pictures are: the original's absolute path and its path
  relative to the catalog; the catalog's previews folder; the
  provider's own keys for finding a preview at request time
  (Lightroom's `id_local` and the file's `id_global`).

The table holds no preview path. A preview's file name carries a digest
that moves on every edit, and Lightroom deletes and rebuilds previews on
its own schedule, so a path render wrote would go stale between syncs.
The applet looks the current one up when asked (§4).

## 4. The `photos` applet

`datalib-applet photos`, a subcommand and a module beside `slack` and
`datalib_contacts` ([`applets.md`](../applets.md)):

```toml
[[applets]]
id = "photos"
command = "datalib-applet photos"
[applets.params]
sources = ["lightroom_main"]
```

It reads each named source's render store as a reader: read-only,
pinned to one commit (`doltlite_raw::open_reader`). It writes nothing.

### The token

Nothing new. Every request reaches the applet through the gateway,
which is behind the per-process API token, the same arrangement as a
Jupyter server's token: the browser holds it as a same-origin cookie,
and an `<img src="/applet/photos/…">` sends it like any other request.
The applet refuses any request without the `X-Datalib-Applet-Secret`
the gateway adds, and any whose `Host` is not its own loopback port
(`gate.rs`, as every applet does). A script fetching a picture sends
`Authorization: Bearer <token>` to the gateway, as for any route.

### Routes

| route | answers |
| --- | --- |
| `GET /photo/<uuid>` | the photo's `photos` row as JSON: everything the card draws |
| `GET /photo/<uuid>/image?size=thumb` | the smallest preview at least 320 px on its long edge |
| `GET /photo/<uuid>/image?size=full` | the largest size of the current preview, which may be smaller than the original (§1); the card says how big it is |
| `GET /photo/<uuid>/original` | the original file, with range requests, so a video can play and seek |

**Which preview**: open `previews.db` read-only, read
`ImageCacheEntry` for the photo's `id_local` to get the current
`(uuid, digest)`, then `PyramidLevel` for its sizes. Read the bytes from
the loose file, or from the matching `level_N` section of the `.lrprev`
for an older catalog. The uuid and digest are checked against
`[0-9A-F-]{36}` and `[0-9a-f]{32}` and the path is built from them under
the previews folder, never taken from anything the request sent.

**When there is no preview**, the original is served if a browser can
show it (JPEG, PNG, GIF, WebP; H.264 video) and refused with a reason if
not (RAW, TIFF, PSD). This is the fallback the request asked for, and
it is made visible rather than quiet: the answer carries
`X-Datalib-Image-From: preview` or `original`, the card says "no
Lightroom preview; showing the original" or "no preview, and the
original is RAW", and each fallback is logged. A source whose
`previews.db` is missing altogether gets one problem row on its Manage
row, not one log line per picture.

**What every answer carries**: the type read from the bytes' first few
bytes, not from the file name; `X-Content-Type-Options: nosniff`;
`Cache-Control: private, no-cache` with an `ETag` of the preview's
digest, since the same URL shows a new picture after an edit.

### Cards

The applet writes two components into its namespace:

- **`comp.photos.photo("<uuid>")`**: the picture at full size with face
  rectangles that can be toggled, and beside it the title, caption,
  capture time, camera and exposure, rating, keywords, people, albums
  and place. Each keyword, person, album and place is a link that opens
  the search on that term (`label:…`, `place:…`). Opening a card never
  writes ([`cards.md`](../cards.md) §"Card source").
- **`comp.photos.browse("<source id>")`**: a contact sheet, the search
  grid on `source:<id>` drawn as thumbnails. It is a search card with a
  different look, not a second list endpoint.

### What the UI needs

- **A thumbnail cell.** A new column type in `datalib_columns`,
  `image`, drawn as a lazy-loaded `<img>` sized to the row. The
  `unified_document_index` applet declares the `image` column with it.
- **Finding the applet that serves `photo`.** The applet's component
  metadata says `{"serves": ["photo"]}`. The UI reads it from
  `/api/frontend` and turns `datalib:photo/<uuid>` into
  `/applet/<that applet's id>/photo/<uuid>/image?…`. This is the first
  row of the kind registry in [`dispatch_on_kind.md`](dispatch_on_kind.md),
  built for one kind, not another branch in an if-chain. When `/data/`
  learns to hand a kind to an applet ([`datalib_uris.md`](datalib_uris.md)
  §3), the address moves and no row changes.
- **Opening a photo row** opens `comp.photos.photo(uuid)` instead of the
  document view, by the same lookup.

## 5. When a photo was taken

The grid needs an instant to sort and filter by; Lightroom stores a wall
clock. The offset comes from, in order:

1. the offset on the XMP's `exif:DateTimeOriginal`;
2. else the offset on `xmp:CreateDate`;
3. else the zone at the photo's GPS fix on that date (needs a
   time-zone boundary map: §9);
4. else the zone the source's config names, a new `capture_time_zone`
   that the wizard fills with the computer's zone when the source is
   added, so a later render does not depend on where the computer is
   that day.

The wall clock always comes from `captureTime`, since that is what the
person edits. `created_at` is written with the offset found, as the
source's own stamp ([AGENTS.md](../../../AGENTS.md) §"Timestamp
convention"), and the `photos` row says which rule supplied it. A source
where rule 4 was used reports one problem row with the count, so "a
third of your photos' times assume Europe/Berlin" is on the Manage row,
not hidden.

A source with `skip_xmp = true` has no XMP, so rules 1 and 2 and every
title are gone. Render reports that once per source.
`lightroom/INGEST.md` §"The XMP question" says the XMP is "wholly
derived from columns that stay"; the example catalog shows it is not
(title, offset), so that section is corrected in the same change.

## 6. H3

`h3o` (BSD-3-Clause, on `deny.toml`'s allow list), a pure-Rust H3.
Render computes the cell at resolutions 4, 6, 8 and 10 for every photo
with a GPS fix and writes them as `Place` terms and in the `photos`
row. Four resolutions are four exact-match lookups: "this region",
"this city", "this neighbourhood", "this block". A map card, when one is
built, turns a click into `place:<cell>` at the zoom it is drawn at.

## 7. Before, after, and what a person notices

| | before | after |
| --- | --- | --- |
| Lightroom source in the grid | one storage row | a row per photo and video, newest taken first, with a thumbnail |
| search | nothing about photos | `label:`, `place:`, `caption:`, `before:`/`after:` on capture time, `source:` |
| opening a photo row | (no rows) | a card with the edited picture at full size and its details |
| free-text box | nothing about photos | still nothing (§9) |
| disk | the mirror | the same, plus a render store of a few hundred bytes per photo; no `.md` files, no copied pictures |
| an existing Lightroom source | | re-renders once on upgrade; downloads nothing |
| a source added from backups only | | rows and details, but every picture comes from the original or not at all: backups carry no previews |

## 8. Order of work

1. **Built.** Catalog 03 with its previews, `previews.db` and originals,
   pinned as one `http_archive` (`lightroom_catalog_03_previews`) beside
   the four catalogs the mirror test pins.
2. **Not needed**: see §3, "A document with no file".
3. **Built.** `photo_schema`, `source_photos` (render store and index),
   the `image`, `latitude` and `longitude` columns, the `Place` and
   `Caption` term kinds behind `place:` and `caption:`, and
   `lightroom_render`, tested against catalog 03
   (`lightroom_render:catalog_03`).
4. **Built**, but one card rather than two: `photos(applet, uuid?)` is
   the contact sheet without a uuid and the photo with one. Both preview
   formats; tested by starting the binary over catalog 03's previews
   (`applets:photos_catalog_03`).
5. **Built.** The `serves` lookup; opening a photo row (the document
   card hands a photo to the applet's card, and beside the search list
   shows its picture); and the grid's Picture column, an `image` column
   the grid shows as soon as a row in the results has one. A Lightroom
   source's Browse card opens with it beside the title.
6. **Time zones by position** (§5 rule 3), once §9 settles the data.

Later: Apple Photos through the same `photos` table; a RAW original's
embedded JPEG when there is no preview; a thumbnail tier copied into the
blob store, so a library on an unplugged drive still shows thumbnails
(about 30–45 KB a photo at 320 px, so 3–4.5 GB per 100,000).

## 9. Open questions

- [ ] **Free text over captions.** Options: leave it to `caption:`
  (this plan); write a small text-only `.md` per photo for qmd, which
  goes against "no markdown on disk" and puts 10⁵ files in qmd, a cost
  nobody has measured; or teach the qmd indexer to take documents from a
  store instead of a glob.
- [ ] **People: labels only, or `with:` too?** A `with:` match would let
  "everything with Campino" cover photos and chats together, and link
  to the contacts app later. Lightroom knows a name, not a handle.
- [ ] **Time zone by position.** The usual boundary map
  (timezone-boundary-builder) is ODbL, which is not on the allow list.
  Without it, rule 3 goes and rule 4 covers more photos.
- [ ] **Reading `previews.db` while Lightroom is open.** The committed
  copy is in rollback-journal mode, not WAL, so a read-only open needs no
  `-shm` file; whether Lightroom keeps it that way while running, and
  how long it holds a write lock, is to try against a running Lightroom
  before step 4 is merged.
- [ ] **"Full size" smaller than the original.** Serve the current
  preview's largest size (this plan: it is the edited picture), or the
  original when it is larger and a browser can show it (bigger, but
  without the person's edits)?
- [ ] **Virtual copies** share one original and so one file uuid; they
  should get their own `ImageCacheEntry` rows. Unverified: no example
  catalog has one.
