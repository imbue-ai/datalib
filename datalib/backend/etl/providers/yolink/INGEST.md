# YoLink Download

The ingest step of a `yolink` group mirrors per-device sensor history
from `us.yosmart.com/download/...` into a doltlite raw store:

```
<data_root>/<group>/ingest/entities.doltlite_db
  yolink_devices    one row per configured device + its resume cursor (last_ts_ms)
  yolink_readings   one row per sample, keyed device#ts_ms#metric
```

Each device is walked forward from its configured `start` date in
`window_days` strides (default 7). Each request asks for one stride plus
`overlap_minutes` (default 5), and a later run resumes from the device's
newest stored reading less that overlap. Each window is a signed-URL CSV
fetched with `curl`; the signing scheme is in `src/ingest/mod.rs`
(`build_signed_url`), reverse-engineered from YoLink's Android client,
since the public API exposes no historical CSVs. A failed window is
skipped and the walk goes on; thirty failures in a row abandon the
device for the run.

Moving a device's `start` earlier re-walks it from the new start (the
starts are recorded in `sync_scope_config`); moving it later than the
stored cursor skips the gap, with a warning.

## Upstream history expires. The mirror is the only durable copy.

**This is the most important thing to know about this provider.** YoLink
serves only a trailing window of history. Ask for anything older and you
get a successful, empty response — no error, no warning, nothing in the
run summary to distinguish "that period had no readings" from "that
period is gone".

Measured on one live account: a backfill from a start five months back
requested every window and every one succeeded, yet all six devices'
first readings landed within 23 minutes of one UTC midnight about 66
days before the fetch — a server-side cutoff, not a windowing bug. The
cutoff fell a day inside a window rather than on a window boundary, and
a later ~56-minute request returned only that window's rows, so the
endpoint honours the requested range. The exact policy (a rolling
~66-day retention is the obvious reading) is not established; a live
probe of an early window, repeated later, would settle it.

### What follows from it

- **Sync cadence is data retention.** A lapse longer than the upstream
  window loses that stretch permanently. Everything already in the
  doltlite store is safe — that is the whole point of keeping a mirror —
  but nothing recovers what was never fetched.
- **`errors=0` does not mean healthy.** An empty window and a quiet
  window are indistinguishable in the summary. The same blind spot hides
  a dead sensor: a device that stops reporting produces successful,
  empty windows forever. On the store measured above, one freezer sat
  silent for 14 days with clean run summaries throughout. Comparing
  `MAX(ts_ms)` per device against wall-clock time is how you notice
  (`$dl` is the shell built below):

  ```sh
  $dl <data_root>/<group>/ingest/entities.doltlite_db \
    "SELECT device_name, datetime(MAX(ts_ms)/1000,'unixepoch') AS last_seen
       FROM yolink_readings GROUP BY device_name ORDER BY last_seen;"
  ```

## Backfilling from an older store

Because history expires upstream, an old raw store from a previous
machine or a retired group can hold readings that no longer exist
anywhere else. Merging one in is a two-table upsert.

Everything below uses the Bazel-built shell, which links the same
doltlite amalgamation the pipeline writes with:

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
```

**Stock `sqlite3` cannot open these files.** Prefer the Bazel target over
a host `/usr/local/bin/doltlite` so the CLI can't silently disagree with
`MODULE.bazel`'s pin.

### Check what you actually have first

The `yolink_readings` primary key is
`{device_name}#{ts_ms}#{metric}` (`schema_raw::reading_id_recipe`), so
rows for the same reading collide by construction and the merge needs no
id rework. Confirm that the backup's ids follow the same recipe:

```sh
# every id in the source matches the current recipe?
$dl <backup>/ingest/entities.doltlite_db \
  "SELECT COUNT(*) AS total,
          SUM(id = device_name || '#' || ts_ms || '#' || metric) AS matching
     FROM yolink_readings;"
```

Then check what the merge would gain and whether the overlap agrees.
`ATTACH` works, so this is one query:

```sh
$dl <data_root>/<group>/ingest/entities.doltlite_db "
ATTACH DATABASE '<backup>/ingest/entities.doltlite_db' AS src;
SELECT 'gained', COUNT(*) FROM src.yolink_readings s
  WHERE NOT EXISTS (SELECT 1 FROM yolink_readings c WHERE c.id = s.id);
SELECT 'overlap', COUNT(*) FROM src.yolink_readings s JOIN yolink_readings c USING(id);
SELECT 'overlap disagreeing on value', COUNT(*)
  FROM src.yolink_readings s JOIN yolink_readings c USING(id) WHERE s.value <> c.value;
SELECT 'same physical devices?', COUNT(*) FROM yolink_devices c JOIN src.yolink_devices o USING(id)
  WHERE c.family_device_id <> o.family_device_id;   -- expect 0
"
```

A non-zero "disagreeing on value" count means the two stores fetched
different values for the same sample and you need to decide which wins
(`DO UPDATE SET value = excluded.value, payload = excluded.payload`
rather than `DO NOTHING`). In the one real case measured, two fetches
seven weeks apart agreed on every one of 61,400 shared ids.

### The merge

Back up first; this mutates the store in place. No sync may be running:
the shell is a second writer. It works on the writer branch and then
moves `main` to it, as every writer does (`etl/README.md` §"A writer
works on its own branch and publishes when it seals"); a commit made on
`main` directly would be dropped by the next sync's seal.

```sh
cp <data_root>/<group>/ingest/entities.doltlite_db{,.pre-backfill}
```

```sh
$dl <data_root>/<group>/ingest/entities.doltlite_db <<'SQL'
SELECT dolt_connect_branch('datalib_writer');
ATTACH DATABASE '<backup>/ingest/entities.doltlite_db' AS src;

INSERT INTO yolink_readings
       (id, payload, device_name, ts_ms, metric, value)
SELECT  id, payload, device_name, ts_ms, metric, value
  FROM src.yolink_readings
 WHERE true
    ON CONFLICT(id) DO NOTHING;

INSERT INTO yolink_readings_bookkeeping
       (id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, volatile_payload, tz_offset)
SELECT  id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, volatile_payload, tz_offset
  FROM src.yolink_readings_bookkeeping
 WHERE true
    ON CONFLICT(id) DO NOTHING;

SELECT dolt_commit('-Am', 'backfill: import history from <backup>');
SELECT dolt_branch('-f', 'main', 'datalib_writer');
SQL
```

Notes on the shape of that statement:

- `WHERE true` is not decoration. Without it SQLite cannot tell whether
  `ON CONFLICT` belongs to the `SELECT` or is the upsert clause; this is
  the documented workaround.
- The bookkeeping sidecar comes along so imported rows keep their
  original `fetched_at_utc` provenance. Drop that second `INSERT` if you'd
  rather they read as never-fetched.
- **It is idempotent.** Running it twice leaves the row count and the
  commit count unchanged; the second run exits non-zero with
  `nothing to commit, working tree clean`, which is `dolt_commit`
  reporting that nothing changed rather than a failure.
- **`yolink_devices` is deliberately untouched.** The imported rows are
  older than the existing resume cursor, so `last_ts_ms` stays at the tip
  and the next sync resumes there instead of re-walking from the
  backfilled start.
- `dolt_log` is the undo — the whole import is one commit.

The next render sees `yolink_readings` changed and re-renders; the plots
then cover the extended range.

### A backup in an older schema

The merge reads only the two `yolink_readings*` tables. To bring the
backup itself to the current shape, point any current yolink step at
it: opening a raw store adds missing tables and columns and commits
`schema: apply DDL`, and refuses anything else
(`etl/README.md` §"Schema self-healing"). If the backup predates the
`tz_offset` bookkeeping column, drop it from the second `INSERT`.

## Config

The ingest step's `[steps.params.api]` table (`yolink_config`) takes
`window_days`, `overlap_minutes` and one `[[steps.params.api.devices]]`
entry per device: `name`, `kind` (`temperature_humidity` or
`watermeter`), `start` (`YYYY-MM-DD`), `family_device_id`, `device_udid`.
`docs/user/config_examples/all_sources.toml` has a worked group. A
device's `name` keys its rows, so renaming one orphans its history.

`family_device_id` and `device_udid` are **per-device read secrets**:
anyone holding the pair can pull that device's entire CSV history (see
`schema_raw.rs`). Never commit a real one. The render step deliberately
keeps them off the rendered page, and says so on the page itself so the
omission doesn't get "fixed" later.

## What the `airvisual` provider does differently, and why this one is left alone

`airvisual`, the other time-series source, made four choices this
provider did not. They are recorded here so nobody mistakes the
difference for an oversight, and because **this store is
deliberately left as it is** — its upstream history expires, so any
schema change here is a migration of the only copy, not a re-download,
and the data is more valuable than the tidiness.

1. **Wide rows, typed columns, no payload, no bookkeeping sidecar.**
   `yolink_readings` is long form — one row per (device, ts, metric),
   each carrying the whole CSV line as JSON, plus a `_bookkeeping` row
   — so a THSensor line is stored twice over, four rows in all. The
   same data as one row per (device, ts) with a `REAL` column per
   metric measured at about a tenth of the size on airvisual's data.
2. **One transaction per device**, not per fetched window. A full
   re-walk here is cheap and idempotent, and every SQL commit rewrites
   the store's tree.
3. **A device has an `id` and a `name`.** `devices[].name` here is
   both the display label and the row key, so renaming a device
   orphans its history (the config doc says so). The step id does not
   help — it names the source, and a source has several devices. The
   fix would be the split the config already makes for groups: a
   per-device `id`, chosen once and read-only in the wizard, keying the
   rows and the grid uuid, beside a free-text `name`.
4. **The device row is written only when it changed.** Here it is
   upserted every run through the sidecar path, which stamps
   `fetched_at_utc` and bumps `attempt_count` on an unchanged row, so
   every run dirties the store and commits. The render does not care
   (it gates on the driver's stale set, not on HEAD), but it is a
   commit and dead chunks per run for nothing.
