# YoLink Download

The ingest step of a `yolink` group mirrors per-device sensor history
from `us.yosmart.com/download/...` into a doltlite raw store:

```
<data_root>/<group>/ingest/entities.doltlite_db
  yolink_devices    one row per configured device: its kind, its `start`, half of its credential
  yolink_readings   one row per sample, keyed device#ts_ms#metric
  coverage          which stretches of each device's history have been looked at, scope `device:<name>`
```

Each run wants, per device, the stretch from its configured `start`
date to the run's pinned now (`DATALIB_DAG_NOW`, not the clock), cut
down at the bottom to the ~66 days YoLink still serves (below). What
is owed is the gaps between that and the `coverage` spans the device
holds, walked oldest first in `window_days` strides (default 7). Each
window is a signed-URL CSV fetched through the shared HTTP layer; the
signing scheme is in `src/ingest/mod.rs` (`window_request`),
reverse-engineered from YoLink's Android client, since the public API
exposes no historical CSVs. A window's readings and the record that
the window was looked at land in one transaction, a window with no
readings included, so a device that has gone quiet is not asked for
its silence again. Each window's request begins `overlap_minutes`
(default 5) before the window, so the newest few minutes of the last
run, which a sensor may not have reported yet, are asked for again;
re-fetched samples upsert over themselves. The walk stops at the next
window when the step is told to stop, and the store is sealed after
each device.

Moving a device's `start` earlier is a gap below what it holds, walked
on the next run; moving it later leaves what was fetched in place and
asks for nothing below the new start.

## When part of a sync fails

Only the store failing fails the step. Everything else costs the thing
that failed, as a row in the store's `problems` table:

| what | key | when it clears |
| --- | --- | --- |
| a device with a window that could not be fetched or parsed (the row names the first); one that cannot be walked (a `start` that is not a date); one abandoned after thirty failed windows in a row; or one with no reading at all whose windows YoLink refused (most likely a wrong id) | `listing:<device>` | the next run that walks it clean, or for the refusals, the first run that gets it a reading |
| a device with no reading in the last day | `silent:<device>` | it reports again |

A failed window is a gap: nothing is written for it, so the next run
asks for it again, before the stretch since the last run. The walk goes
on past it, up to thirty failures in a row per device per run, after
which the device is left for the next run. A window asked for again
after YoLink has expired it answers empty and is covered; the readings
it held are gone, so retry within the retention window.

Some failures of a device that has readings have nothing a retry could
fetch, and are covered as if the window had answered empty: a 404 or
410, and a client error (not a timeout or a rate limit) on a window
ending before the device's first reading, which is a `start` that
predates the device. A device with no reading at all cannot tell a
window from before it was deployed from an id YoLink does not know, so
its refused windows stay gaps and count toward the budget, and if the
walk still has no reading at its end the device gets the `listing:`
row above. Once a run gets a reading, the windows refused before it are
the time before the device was deployed and are covered then.

The `listing:` and `silent:` rows are replaced whole at the end of each
run. A run told to stop keeps the `listing:` rows it found before the
stop, clears none, and leaves the `silent:` rows as the last run did.

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
  window are indistinguishable in the summary. A dead sensor is the
  same: a device that stops reporting answers every window with an
  empty body (not even the CSV header), which reads as no readings. On
  the store measured above, one freezer sat silent for 14 days with
  clean run summaries throughout. So every run compares each device's
  newest reading against the clock, and one more than a day old is a
  `silent:<device>` warning in `problems`, on the Manage row, until the
  device reports again. To see every device's last reading (`$dl` is
  the shell built below):

  ```sh
  $dl -readonly <data_root>/<group>/ingest/entities.doltlite_db \
    "SELECT device_name, datetime(MAX(ts_ms)/1000,'unixepoch') AS last_seen
       FROM yolink_readings GROUP BY device_name ORDER BY last_seen;"
  ```

## Backfilling from an older store

Because history expires upstream, an old raw store from a previous
machine or a retired group can hold readings that no longer exist
anywhere else. Merging one in is a two-table upsert.

Everything below uses the doltlite shell as `$dl`
([`docs/dev/doltlite.md`](/docs/dev/doltlite.md) has where to get it):

```sh
bazelisk build //third-party/doltlite:doltlite
dl=bazel-bin/third-party/doltlite/doltlite
```

### Check what you actually have first

The `yolink_readings` primary key is
`{device_name}#{ts_ms}#{metric}` (`schema_raw::reading_id_recipe`), so
rows for the same reading collide by construction and the merge needs no
id rework. Confirm that the backup's ids follow the same recipe:

```sh
# every id in the source matches the current recipe?
$dl -readonly <backup>/ingest/entities.doltlite_db \
  "SELECT COUNT(*) AS total,
          SUM(id = device_name || '#' || ts_ms || '#' || metric) AS matching
     FROM yolink_readings;"
```

Then check what the merge would gain and whether the overlap agrees.
`ATTACH` works, so this is one query:

```sh
$dl -readonly <data_root>/<group>/ingest/entities.doltlite_db "
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
- **`yolink_devices` and `coverage` are deliberately untouched.** The
  next sync still owes only the gaps in what this store had looked at;
  the imported readings sit below what YoLink still serves, so nothing
  would ask for them anyway.
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
2. **One transaction per device**, not per fetched window. Here a
   window's readings and its coverage have to land together, which is
   what makes a cut-off run resumable, and every SQL transaction
   rewrites the pages it touches
   ([doltlite.md § What a write costs](/docs/dev/doltlite.md#what-a-write-costs)).
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
