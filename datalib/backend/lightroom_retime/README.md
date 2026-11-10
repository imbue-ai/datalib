# datalib-lightroom-retime

Applies a file of capture-time edits to a Lightroom Classic catalog, so
each photo carries the time it was taken in its own time zone
([`docs/dev/plans/lightroom_local_time.md`](../../../docs/dev/plans/lightroom_local_time.md)).
Working out the right times is someone else's job, usually an agent's;
this tool only applies them and refuses anything it cannot apply
exactly. It is not shipped yet. Build and run it from a checkout:

```sh
bazelisk build //datalib/backend/lightroom_retime:datalib-lightroom-retime
bazel-bin/datalib/backend/lightroom_retime/datalib-lightroom-retime <catalog.lrcat> <edits.jsonl>          # prints what would change
bazel-bin/datalib/backend/lightroom_retime/datalib-lightroom-retime <catalog.lrcat> <edits.jsonl> --write  # applies it
```

`-` reads the edits from standard input, and `--json` reports one JSON
object per edit (`status` is `write`, `already_done` or `refused`, with
a `reason`), then one for the run. `--help` has the format, what is
written and the exit status.

## The edits file

JSON Lines: one object per line, one line per photo.

| field | holds |
|---|---|
| `id_global` | the photo's `Adobe_images.id_global` (a UUID) |
| `expected` | its `captureTime` exactly as read when the edit was made |
| `capture_time` | the target: the local clock time with its offset, `2024-07-01T14:00:00.000+02:00`, to the millisecond at most |
| `why` | optional: one line on what gave this time |

```json
{"id_global":"FFBB4B7B-CD62-4012-8074-3B10D8CB3A6A","expected":"2024-07-01T12:00:00.000","capture_time":"2024-07-01T14:00:00.000+02:00","why":"camera clock on UTC"}
```

## What it writes

Each place Lightroom keeps a capture time feeds one thing it does
([`RESULTS.md`](../etl/providers/lightroom/tests/capture_time/RESULTS.md)
§"Round 5"), so an edit writes all three:

- `Adobe_images.captureTime`, the clock time with no offset
  (`2024-07-01T14:00:00.000`): the sort.
- `exif:DateTimeOriginal` and `photoshop:DateCreated` in the catalog's
  XMP, with the offset: what the Metadata panel shows, and what Edit
  Capture Time shifts from. A namespace the XMP lacks is declared; the
  rest of the XMP is left byte for byte, and the result is parsed back
  before it is written, since Lightroom reads XMP it cannot parse as
  empty.
- `AgHarvestedExifMetadata.dateYear`, `dateMonth` and `dateDay`, from the
  local date: the day the Library filter files the photo under.

Where the clock time moves and Lightroom has kept no first time, it also
sets `originalCaptureTime` to the time it replaced. After writing it
moves `<catalog> Helper.lrdata`, the cache the Metadata panel reads,
aside to `<catalog> Helper.lrdata.bak_timestamps_<time>`; Lightroom builds
a new one from the catalog on its next open. It never touches the photo
files.

## What keeps it safe

- **Nothing is written without `--write`.**
- **It refuses a catalog Lightroom has open** (`<catalog>.lock`).
- **All or nothing.** If any line is refused, nothing is written. A line
  is refused when its `captureTime` is no longer `expected`, so a file
  made from an older reading never overwrites a newer edit; when the
  photo is missing or has no XMP; or when its XMP will not take the
  dates cleanly.
- **A backup first**: the catalog as it was, beside it as
  `<catalog.lrcat>.bak_timestamps_<time>`, numbered on if that is taken.
- **Running it again is safe**: a photo whose `captureTime`, XMP dates
  and day columns already have its target is counted as done, and a run
  with nothing to do writes nothing, backup included.

It writes with datalib's own SQLite, doltlite, which reads and writes
plain SQLite files. `apply_test` builds a catalog with stock SQLite, as
Lightroom's is, and reads the result back with stock SQLite.

## Not covered

- **Read Metadata from Files puts a file's times back**, since this never
  writes the files. Lightroom warns before it does.
- **Virtual copies** are separate `Adobe_images` rows; each needs its own
  line.
- **Lightroom's sync to its cloud** keeps change counters this does not
  touch; whether a synced catalog notices the edits is not measured.
