# What Lightroom Classic did with each case

Lightroom Classic 15.6 (catalog version 1504001) on a Mac set to Berlin
time, with the files from `make_cases.py` (A–E, V) and `make_more_cases.py`
(F); `manifest.csv` beside each set says what every file holds.
[`INGEST.md`](../../INGEST.md#how-lightroom-keeps-a-capture-time) states the
rules these show; this is the case-by-case record, read from the catalog
with `read_catalog.py` after each round of the README.

## Reading and sorting (round 1)

- **The sort is `Adobe_images.captureTime`'s clock time only.** Equal times
  fall back on file name. B01…B12, a minute apart in reality, sorted B03 B06
  B09 B12 B01 B07 B02 B05 B08 B11 B04 B10. Not a text sort: D05, whose
  `captureTime` is the only one with an offset (`…14:00:00+02:00`), sorts
  among the 14:00s by name, in both rounds.
- **`captureTime` carries no offset**, except when the only date was EXIF
  `ModifyDate` + `OffsetTime` (D05): then it is stored with one
  (`2024-07-03T14:00:00+02:00`).
- **The offset is kept in the catalog's XMP** (`exif:DateTimeOriginal`,
  `photoshop:DateCreated`, `xmp:ModifyDate`), never shown and never sorted on.
- **XMP inside the file beats EXIF** (D01 took the XMP's San Francisco time
  over the EXIF's Berlin one).
- XMP-only `DateTimeOriginal` (D03), `photoshop:DateCreated` alone (D11) and
  EXIF `CreateDate` alone (D04) are all used.
- **GPS time is never used**; kept in the XMP as `exif:GPSTimeStamp` (D07).
- **No date at all: the file's modified date, converted to UTC**, offset
  dropped (2001-01-01 00:00 +01:00 became `2000-12-31T23:00:00`). Those items
  have a UTC clock time; the rest a local one.
- Subseconds are kept and sorted (D10b .10 before D10a .90).
- The two 02:30s of the night clocks go back (C01 +02:00, C02 +01:00) get the
  same `captureTime` and tie.
- **Videos: the QuickTime date is shown as written, with no UTC conversion.**
  A spec-correct UTC date (V01, V04) shows as 12:00 for a 14:00 Berlin
  moment; its XMP says `xmp:CreateDate=…12:00:00Z`. A camera that wrote local
  time as UTC (V02) shows 14:00. An iPhone-style `Keys:CreationDate` with an
  offset (V03) is used and its offset kept. An empty QuickTime date (V05)
  becomes 1904-01-01, not the file date. An XMP `DateTimeOriginal` (V06) wins
  over an empty QuickTime date.

## Edit Capture Time (round 2)

- **The clock time moves and the recorded offset stays.** `originalCaptureTime`
  keeps the pre-edit time; `xmp:ModifyDate` keeps the old one too.
  - E01, +1 h on 14:00 +02:00: now 15:00 +02:00. The moment moved an hour.
  - E03, set to 14:00 on 05:00 −07:00: now **14:00 −07:00**, 21:00 UTC. The
    offset no longer belongs to the time.
  - E02/E04 (no offset): the new clock time, still no offset.
- **A video's edit adds an `exif:DateTimeOriginal` with no offset** (E05:
  `14:00:00.000`) and leaves `xmp:CreateDate=…12:00:00Z` as it was.
- E06 (no dates), set to 1968-05-01 12:00: `DateTimeOriginal` 1968, no offset.
  Its `originalCaptureTime` is the import's fallback, the file date in UTC
  (`2000-12-31T23:00:00`).
- **The order after the edits (round 2) is exactly (clock time, file name)**:
  every move is the edited `captureTime`. E03, E04 and E05, three different
  edits of the same moment, now tie at 14:00 with E01X and E02X, which is
  "lined up" as Lightroom sees it; E01 and E02 (+1 h) sit an hour after.

## Save Metadata to Files (round 3)

- **Every JPEG was rewritten; no video was touched**, and no `.xmp` sidecar
  was made for them. A video's edits exist only in the catalog.
- **EXIF `DateTimeOriginal` in the file is rewritten from the catalog**, with
  `OffsetTimeOriginal` kept as it was: E03's file now says 14:00 −07:00. D01's
  file had Berlin in EXIF; it now has the XMP's San Francisco time there.
- EXIF `ModifyDate` keeps the pre-edit time. Each file's modified date
  becomes the save time.
- **The Metadata panel's "Capture Date/Time" is not `captureTime`.** D05 (no
  `DateTimeOriginal`) showed as Capture Date/Time the moment of the save, to the
  millisecond: its file's new modified date (`AgLibraryFile.modTime`,
  seconds since 2001), beside Date Time, Capture Date and Capture Time all saying
  2024-07-03 14:00 and Date Time Original empty. It still sorts at 14:00.
- **No displayed time uses the offset** (Mac set to Berlin). A04 (05:00
  −07:00 in the XMP) shows 05:00 in Date Time, Capture Date/Time, Date Time
  Original and Capture Time, not 14:00. E03 (14:00, stale −07:00) shows
  14:00 in Capture Date/Time, Date Time Original and Capture Time, not
  23:00; its Date Time (EXIF `ModifyDate`) still shows the pre-edit 05:00.
  E03 was made with only `DateTimeOriginal`: the catalog's XMP had
  `xmp:ModifyDate=05:00-07:00` already at import (E03X too), copied from the
  capture time, and Save Metadata wrote it into the file as IFD0
  `ModifyDate` with `OffsetTime`.
- `AgLibraryFile.importHash` starts with a capture time in seconds since
  2001, the clock read as UTC; for D05 it is `-3600`, the file's date at
  import, not the `ModifyDate` its `captureTime` came from.

## Round 4: where an offset in `captureTime` comes from

The F files, imported with the Mac in Berlin (+01:00). AVIs with an MPEG-4
Part 2 stream were refused as unrecognizable; `make_more_cases.py` now
writes Motion JPEG and PCM sound, which imported.

- **A modify date's offset is copied into `captureTime`**, negative too:
  EXIF `ModifyDate` + `OffsetTime` (F01) and XMP `xmp:ModifyDate` alone (F02)
  both gave `2024-08-01T14:00:00-07:00`.
- **Every other source drops it**: `xmp:CreateDate` with an offset (F03),
  IPTC date and time (F07, kept as `photoshop:DateCreated` with the offset),
  iPhone-style `Keys:CreationDate` (F26) all gave `…T14:00:00.000`. An
  offset-less `DateTimeOriginal` wins over an `xmp:ModifyDate` with one (F08).
- **`xmp:MetadataDate` alone is ignored** (F06): the file-date fallback.
- **No seconds is not kept**: `xmp:CreateDate` `…T14:00` (F04, F05) became
  `…T14:00:00.000`, and Lightroom wrote the seconds into its XMP.
- **A JPEG, PNG or TIFF with no date gets its file's modified date in UTC**,
  daylight saving applied: summer 23:00 +02:00 → `21:00`, winter 23:00 +01:00
  → `22:00` (F10–F13).
- **An MPG or VOB with no date gets the time of the import, in the Mac's zone
  with its offset** (F23, F24: three seconds before the import record's time), and Lightroom writes the same value as `xmp:ModifyDate`. Not
  the file's modified or created date (both 2024-08-01). So such a "capture
  time" is when the file was imported.
- **Every video gets `xmp:ModifyDate` = the import time** (F25–F27) even
  where it has a capture time. A MOV or MP4 with empty QuickTime dates gets
  1904 even when its own XMP has `xmp:ModifyDate` (F25) or `xmp:CreateDate`
  (F27): the XMP is replaced, unlike round 1's V06 (`DateTimeOriginal`).
- `AgLibraryFile.importHash` uses the capture time when there is one, else the
  file's modified date (F01/F02 `-3600`, F23 the summer date).
- **The AVIs, remade as Motion JPEG + PCM, imported at 20:41.** With no date
  (F20 summer, F21 winter file dates) they got the import time with the Mac's
  offset, and the same `xmp:ModifyDate`: AVI
  behaves like MPG and VOB. With a RIFF creation date that has no time (F22,
  `ICRD` 2024-08-01) `captureTime` is the bare date `2024-08-01`, no time at
  all, and `xmp:CreateDate=2024-08-01`. The import time was 3 s before the
  import record's in the first import and 15 s before it in this one.
- **All 80 after round 4 sort by (clock time, file name), offsets ignored.**
  F01 and F02 (`14:00:00-07:00`, 21:00 UTC) sit among the 14:00s, not with
  F10's 21:00; F23/F24 and F20/F21 (the import day, +01:00) sort last; F22's bare date
  sorts as midnight, first of 2024-08-01; F25/F27 sort first, at 1904.

## Where an offset is stored

- Searching every text column of the catalog after round 4 for a time with
  an offset (or `Z`) found, besides `Adobe_images.captureTime` (7 rows):
  `AgLibraryImport.importDate` (each import, the Mac's +01:00),
  `AgLibraryImageChangeCounter.changedAtTime` (every image, UTC), and
  `AgLibraryIPTC.caption` only because the captions name offsets. The XMP
  blobs are compressed, so they are read separately (`read_catalog.py`).
