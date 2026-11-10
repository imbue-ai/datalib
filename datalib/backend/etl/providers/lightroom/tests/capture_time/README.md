# Capture-time cases for Lightroom Classic

Black test photos and videos whose capture times and time zones are written
in many different ways, and the steps to see how Lightroom Classic reads,
sorts, shows and edits them. What they showed is in
[`RESULTS.md`](RESULTS.md), case by case, and as rules in
[`INGEST.md`](../../INGEST.md#how-lightroom-keeps-a-capture-time).

Every file is made up: no real photo goes near these scripts, and
`read_catalog.py` is only for a catalog of these files.

| script | makes |
|---|---|
| `make_cases.py` | 60 files, groups A–E and V |
| `make_more_cases.py` | 20 files, group F: where an offset in `captureTime` comes from |
| `read_catalog.py` | prints each case's `captureTime`, `originalCaptureTime` and XMP dates |

Each generator needs exiftool, ImageMagick (`magick`) and ffmpeg, refuses a
non-empty directory, and writes `manifest.csv` (what each file holds, the
moment it stands for, what it asks) and `hashes.txt` (each file's sha256 as
made, so a later look can tell which files Lightroom rewrote):

```sh
python3 -I make_cases.py ~/lr-tz-test/media
python3 -I make_more_cases.py ~/lr-tz-test/media4
```

Every file is dated 2001-01-01 on disk, unless its case is about the file's
date, so a time Lightroom took from the file stands out.

## The groups

- **A, one moment (2024-07-01 12:00 UTC), many spellings**: with and without
  an offset, in five zones, some with GPS that agrees or disagrees.
- **B, sort order**: twelve moments a minute apart, B01 first, each spelled
  one of six ways.
- **C, Berlin's daylight-saving edges**: the 02:30 that happens twice in
  October, and the one that never happens in March.
- **D, fallbacks and conflicts**: EXIF and XMP disagreeing; a time only in
  the XMP, the digitized date, the modify date or the GPS stamp, or nowhere;
  two photos in the same second told apart by subseconds.
- **E, to edit** with Edit Capture Time, each with an untouched twin (`E01`
  and `E01X` start identical).
- **V, videos (2024-07-05 12:00 UTC)**: QuickTime dates as UTC and as local
  time, an iPhone-style file, an MP4, one with no dates, one with an XMP
  date; `V00` and `V99` are photos a minute either side.
- **F, where an offset comes from (2024-08-01 14:00 −07:00)**: one date
  source each (modify date, `xmp:CreateDate` with and without seconds,
  `xmp:MetadataDate`, IPTC); no date at all with a summer or winter file
  date, as JPEG, PNG and TIFF; AVI, MPG and VOB with no date; MOV and MP4
  with only an XMP date.

## Running a round

1. **Make a new catalog** (File › New Catalog…), never your real one, with
   Catalog Settings › Metadata › "Automatically write changes into XMP"
   off.
2. **Import** with **Add**, no presets, and **"Don't Import Suspected
   Duplicates" unticked**: the E twins and two F files look alike.
3. View › Sort › **Capture Time**, with the grid's labels showing File Name
   and Capture Date/Time. Note the order (a screenshot, or the images'
   `id_global`s in grid order) and, for the cases that ask, the Metadata
   panel.
4. **Edit Capture Time**, one image selected at a time (with several
   selected, Lightroom shifts them all by the active one's change):

   | file | edit |
   |---|---|
   | E01, E02 | Shift by set number of hours: +1 |
   | E03 | Adjust to a specified date and time: 2024-07-04 14:00:00 |
   | E04, E05 | Shift by set number of hours: +2 |
   | E06 | Adjust to a specified date and time: 1968-05-01 12:00:00 |

5. **Save Metadata to Files** with everything selected; compare the files
   against `hashes.txt` to see which were rewritten.
6. Read the catalog after each step. With Lightroom open, copy the
   `.lrcat` with its `-wal` and `-shm`, checkpoint the copy, then
   `python3 -I read_catalog.py <copy> [prefix]`.
