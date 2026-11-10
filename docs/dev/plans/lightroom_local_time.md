# Lightroom photos in their local time

**Status: proposal (2026-11-09), not built.** What Lightroom does with a
capture time today, measured, is in
[`lightroom/INGEST.md`](../../../datalib/backend/etl/providers/lightroom/INGEST.md#how-lightroom-keeps-a-capture-time);
this plan builds on it and does not repeat it.

## 1. The goal

- **datalib treats the Lightroom catalog as correct**, time zones
  included, and fills a time zone in where the catalog has none.
- **There is a good way to make a catalog correct**, run by its owner.
- **Each photo carries the time zone it was taken in, and Lightroom shows
  its local time.**
- **Only photos in the same folder are expected to sort correctly against
  each other** in Lightroom, so a folder is expected to be in one time
  zone. Across folders, only datalib compares, by the moment.

Decided with the catalog's owner:

- **The catalog is the single source of truth.** A fix writes the
  catalog only, never the photo files (so no Save Metadata to Files).
- **Videos move to local time** like photos, though most are stored on a
  UTC clock today (about 9,500 in the one real catalog measured).
- **The tool that writes the catalog lives in its own repository.** It
  takes a CSV of photo id to timestamp and applies it; working out the
  CSV is the hard part, and an agent does it, from file names, other
  photos in the folder, the phone's times and GPS.
- **"One folder, one time zone" holds mostly, not always**, so a folder
  that crosses zones has to be found and decided photo by photo.
- **The time-zone boundary map may be tried**, though it is ODbL (§7).

## 2. What a correct photo is

1. **`captureTime` is the wall-clock time where the photo was taken.**
   Lightroom shows `captureTime`'s clock and nothing else, so it then
   shows local time with no change to Lightroom.
2. **The catalog carries the matching UTC offset**, so the moment is
   known: in the XMP's `exif:DateTimeOriginal`, and perhaps in
   `captureTime` itself (Lightroom stores and ignores one there, §4).
3. **A folder's photos share a time zone**, so Lightroom's sort (clock
   time, then file name) is also the order they were taken in, except in
   the repeated hour of the night the clocks go back.

An unedited photo from a camera that records an offset and was set to the
local zone is already correct. What is not: a photo with no offset; an
edited photo whose kept offset belongs to another zone; a video on a UTC
clock; a "capture time" that is really a save or import time
(`INGEST.md` §"The offset").

## 3. The pieces

| piece | where | does |
|---|---|---|
| the render reads the catalog as correct, and reports where it is not | datalib | §5 |
| the evidence an agent works from | datalib | §6 |
| the CSV: one target time per photo | an agent, with the owner | §6 |
| the applier: writes the CSV into the catalog | its own repository | §8 |

datalib never writes the catalog: it mirrors it, and reads the mirror.
The loop is: sync; read the problems and the evidence; make a CSV;
review it; apply it with Lightroom closed; sync again and watch the
problems go.

## 4. Phase 0: what can write a capture time into the catalog

Measured on the test catalog, with the cases in
`lightroom/tests/capture_time/`. Each answer decides how §8 works.

- **Can a plugin set a photo's capture time?** Jeffrey Friedl's
  Bag-o-Goodies page says Lightroom does not let plugins change a photo's
  date. If that still holds, the applier cannot be a plugin, and writes
  the `.lrcat` directly with Lightroom closed.
- **Does a direct edit hold?** With Lightroom closed, change
  `captureTime`, the XMP's dates and the harvested date columns behind
  the Library filter (`AgHarvestedExifMetadata.dateYear` and its
  neighbours). Does Lightroom show and sort by the new time, keep it, and
  not rebuild it from the file? What does it need besides
  (`AgLibraryImageChangeCounter`, search indexes)?
- **An offset in `captureTime`** (`…T14:00:00+02:00`), which Lightroom
  already writes in one case: does it survive Lightroom's own edits, a
  Read Metadata, a re-import? If it does, the catalog carries the zone in
  the one column everything reads.
- **Videos**: the same for a MOV and an MP4, whose times Lightroom
  otherwise takes from QuickTime's UTC field.
- **DNG and HEIC**, which the cases have none of; an Adobe feature
  request says Lightroom drops `OffsetTime` from DNGs.
- **What an export writes** from a corrected photo, since an export is
  how a photo leaves a catalog that is the source of truth.

## 5. Phase 1: the render trusts the catalog

In `lightroom_render/src/photo.rs::capture_time`:

- **The clock from `captureTime`**, every shape of it (zero to seven
  fractional digits, no seconds, a bare date).
- **The offset from the catalog, edited or not**: `captureTime`'s own,
  else the XMP's `exif:DateTimeOriginal`. No guessing about what an edit
  was for: once the catalog is correct, its offset is right.
- **A missing offset from the folder**: the offset of the nearest-in-time
  photo in the same folder that has one, a new `OffsetFrom::Folder`, but
  only where the folder's offsets agree (allowing a daylight-saving
  change). Then `capture_time_zone`, then UTC.
- **A problem row wherever the catalog is not yet correct**, so the
  Manage row is the to-do list:
  - a folder whose offsets name more than one zone;
  - an offset equal to the photo's `xmp:ModifyDate`, a save or import
    time rather than a capture time;
  - a video on a UTC clock (`xmp:CreateDate` ending `Z`) in a folder of
    local times;
  - an edited photo (`originalCaptureTime` set) whose offset is the one
    from before the edit, in a folder where other photos say otherwise;
  - a photo read as UTC.

This is useful before any catalog is fixed: it is right for the photos
that are already correct, and it says where the rest are.

## 6. Phase 2: the evidence, and the CSV

An agent makes the CSV from evidence datalib already holds in the
mirror, without opening the catalog. datalib's part is a documented way
to get, per photo:

- `id_global`, folder, file name, camera model and serial;
- `captureTime`, `originalCaptureTime`, the XMP's dates and offsets, and
  where the render's offset came from;
- GPS, where there is any;
- the problem rows from §5.

The agent works per folder: pick the folder's zone (from the offsets
the photos agree on, the phone's photos those days, GPS, the folder's
name), find each camera's clock error against the phone, then give each
photo its local time. A folder that crosses zones is decided photo by
photo. Videos whose time is an import time have no evidence of their own
and are left for the owner, or dated from their neighbours with a note.

**The CSV**, one row per photo to change:

| column | holds |
|---|---|
| `id_global` | the photo's uuid in the catalog |
| `expected` | its `captureTime` as the agent read it |
| `capture_time` | the target: local clock time with its offset, `2024-07-01T14:00:00.000+02:00` |
| `why` | one line: which rule or evidence gave it |

The applier refuses a row whose `captureTime` is no longer `expected`, so
a CSV made from a mirror older than the catalog never overwrites a newer
edit. Before applying, a check over the CSV: every offset is valid for
its zone on that date, and a folder's rows agree on a zone unless marked.

## 7. The time-zone map

The usual map of time-zone boundaries, timezone-boundary-builder, is
ODbL, and datalib takes only permissively licensed code and data
(`datalib/backend/deny.toml`; the photos plan's §9). Once the catalog is
correct, datalib needs no map: the offset is in the catalog. So the map
belongs with the agent and the applier, in their own repository, where
its share-alike terms apply to that repository's choices, not datalib's.
Whether a lookup's result (an offset per photo, written into the
catalog) is a work its terms reach is for whoever publishes that
repository to check.

## 8. Phase 3: the applier

In its own repository. Reads the CSV; with Lightroom closed (it refuses
while the catalog's lock file is there), backs the catalog up, then per
row writes what phase 0 found Lightroom needs: `captureTime`, the XMP's
`exif:DateTimeOriginal` and `photoshop:DateCreated` with the offset, the
harvested date columns. It leaves `originalCaptureTime` as Lightroom's
record of the first time, setting it only where it is empty. It has a
dry run that prints what would change, and is safe to run twice.

## 9. Open questions

- [ ] **Plugin or direct edit** (phase 0).
- [ ] **Where the offset lives**: `captureTime`, the XMP, or both.
- [ ] **Virtual copies** share a file but are separate `Adobe_images`
  rows; the CSV should name each, and the check should require they
  agree.
- [ ] **New imports**: a camera left on home time keeps coming. The §5
  problems find each new folder that needs work; whether to do more is
  for after.
- [ ] **Read Metadata from Files** is likely to put the files' old times
  back over a corrected catalog, since the fix never writes the files.
  Not measured, nor when Lightroom warns first; phase 0 should, and until
  then it should not be run on corrected photos.
