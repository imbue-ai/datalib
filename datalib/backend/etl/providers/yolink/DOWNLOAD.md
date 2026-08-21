# YoLink Download

`datalib-step download yolink` mirrors per-device sensor history from
`us.yosmart.com/download/...` into a doltlite raw store. One
forward-marching window per request:

```
<data_root>/<stanza>/raw/entities.doltlite_db
  yolink_devices    one row per configured device + its high-water cursor
  yolink_readings   one row per sample, keyed device#ts_ms#metric
```

Each device is walked from its configured `start:` date in `window_days`
strides (default 7, with `overlap_minutes` of deliberate re-fetch at each
boundary), and each window is a signed-URL CSV fetched with `curl`. See
`src/download/mod.rs` for the signing scheme — YoLink's public API does
not expose historical CSVs, so it was reverse-engineered from their
Android client.

## Upstream history expires. The mirror is the only durable copy.

**This is the most important thing to know about this provider.** YoLink
serves only a trailing window of history. Ask for anything older and you
get a successful, empty response — no error, no warning, nothing in the
run summary to distinguish "that period had no readings" from "that
period is gone".

Measured against a live store on 2026-08-21. A backfill configured with
`start: 2026-03-29` and run on 2026-08-20 returned:

```
download yolink: devices=6 windows=126 readings=150882 errors=0 requests=126
```

126 windows is 21 per device × 6 devices, which is exactly
`ceil((2026-08-20 − 2026-03-29) / 7 days)` — every window from the
configured start was requested and every one succeeded. Yet the earliest
row in the store was **2026-06-15**, roughly 66 days before the fetch.
Every one of the six devices' first reading landed within 23 minutes of
`2026-06-15 00:00:00 UTC`.

### Why that is upstream and not a bug here

Three independent checks, worth repeating if you ever suspect the
windowing:

1. **Cold start really does begin at `start_ms`.** `resume_cursor`
   returns `(start_ms, Normal)` when there is no watermark. Nothing
   skipped ahead.
2. **The endpoint honors the requested range.** A later incremental run
   asked for a ~56-minute window and got 42 rows. Had the endpoint
   ignored the start bound and returned whatever it held, that single
   request would have returned the entire ~150k-row history.
3. **The cutoff does not align to a window boundary.** With
   `start: 2026-03-29` and a 7-day stride, window #11 spans
   `2026-06-14 00:00 .. 2026-06-21 00:05`. The first row is
   `2026-06-15 00:00:09` — **24 hours inside that window**. A windowing
   or cursor bug would put the boundary at 2026-06-14; it doesn't.

Six devices starting to report inside the same 23 minutes, at a UTC date
boundary, is a server-side cutoff rather than six installation events —
especially since that instant was 02:00 local for the operator. What
this evidence does *not* establish is the exact policy (a rolling ~66-day
retention is the obvious reading, but "the devices genuinely began
reporting then" is not excluded from the mirror alone). Settling it needs
a live probe: request an early window today and check whether the
earliest served timestamp has moved forward since.

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
  `MAX(ts_ms)` per device against wall-clock time is how you notice:

  ```sh
  $dl <data_root>/<stanza>/raw/entities.doltlite_db \
    "SELECT device_name, datetime(MAX(ts_ms)/1000,'unixepoch') AS last_seen
       FROM yolink_readings GROUP BY device_name ORDER BY last_seen;"
  ```

## Backfilling from an older store

Because history expires upstream, an old raw store from a previous
machine or a retired stanza can hold readings that no longer exist
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
`{device_name}#{ts_ms}#{metric}` (`schema_raw::reading_id_recipe`) and
has been stable across schema generations, so rows for the same reading
collide by construction and the merge needs no id rework. Confirm rather
than assume:

```sh
# every id in the source matches the current recipe?
$dl <backup>/raw/entities.doltlite_db \
  "SELECT COUNT(*) AS total,
          SUM(id = device_name || '#' || ts_ms || '#' || metric) AS matching
     FROM yolink_readings;"
```

Then check what the merge would gain and whether the overlap agrees.
`ATTACH` works, so this is one query:

```sh
$dl <data_root>/<stanza>/raw/entities.doltlite_db "
ATTACH DATABASE '<backup>/raw/entities.doltlite_db' AS src;
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
seven weeks apart produced a byte-identical overlap — 61,400 shared ids,
zero disagreements on either `value` or `payload` — so the direction did
not matter.

### The merge

Back up first; this mutates the store in place.

```sh
cp <data_root>/<stanza>/raw/entities.doltlite_db{,.pre-backfill}
```

```sh
$dl <data_root>/<stanza>/raw/entities.doltlite_db <<'SQL'
ATTACH DATABASE '<backup>/raw/entities.doltlite_db' AS src;

INSERT INTO yolink_readings
       (id, payload, device_name, ts_ms, metric, value)
SELECT  id, payload, device_name, ts_ms, metric, value
  FROM src.yolink_readings
 WHERE true
    ON CONFLICT(id) DO NOTHING;

INSERT INTO yolink_readings_bookkeeping
       (id, fetched_at, attempt_count, last_attempt_at, last_error, volatile_payload)
SELECT  id, fetched_at, attempt_count, last_attempt_at, last_error, volatile_payload
  FROM src.yolink_readings_bookkeeping
 WHERE true
    ON CONFLICT(id) DO NOTHING;

SELECT dolt_commit('-Am', 'backfill: import history from <backup>');
SQL
```

Notes on the shape of that statement:

- `WHERE true` is not decoration. Without it SQLite cannot tell whether
  `ON CONFLICT` belongs to the `SELECT` or is the upsert clause; this is
  the documented workaround.
- The bookkeeping sidecar comes along so imported rows keep their
  original `fetched_at` provenance. Drop that second `INSERT` if you'd
  rather they read as never-fetched.
- **It is idempotent.** Running it twice leaves the row count and the
  commit count unchanged; the second run exits non-zero with
  `nothing to commit, working tree clean`, which is `dolt_commit`
  reporting that nothing changed rather than a failure.
- **`yolink_devices` is deliberately untouched.** The imported rows are
  older than the existing watermark, so `last_ts_ms` stays at the tip
  and the next sync resumes there instead of re-walking from the
  backfilled start.
- `dolt_log` is the undo — the whole import is one commit.

The render step notices HEAD moved and re-renders on its own; the plots
then cover the extended range.

### Porting a store from an older schema generation

Not required for the merge — that reads only the two `yolink_readings*`
tables — but if you want the backup itself to stand as a valid
current-format store, note that `doltlite_raw::open` already
self-heals: it applies the current DDL with `IF NOT EXISTS`, runs
`reconcile_table_schema` to add missing columns, and commits
`schema: apply DDL`, leaving prior commits intact. Pointing any current
yolink step at the old store is enough.

By hand it is the same thing. Between the generation that wrote
`extract yolink …` commits and today, the entire delta was one table:

```sh
$dl <backup>/raw/entities.doltlite_db "
CREATE TABLE IF NOT EXISTS sync_scope_config(
  scope TEXT PRIMARY KEY, config TEXT NOT NULL, updated_at TEXT NOT NULL);
SELECT dolt_commit('-Am', 'schema: apply DDL');"
```

Diff the schemas before trusting that for any particular pair of stores:

```sh
diff <($dl <backup>/raw/entities.doltlite_db ".schema" | sort) \
     <($dl <data_root>/<stanza>/raw/entities.doltlite_db ".schema" | sort)
```

## Config

See `docs/user/config_examples/all_sources.toml` for a worked
`yolink.download` + `yolink.render` stanza pair (`configs/dag_example.toml`
does not carry one).

`family_device_id` and `device_udid` are **per-device read secrets**:
anyone holding the pair can pull that device's entire CSV history (see
`schema_raw.rs`). Never commit a real one. The render step deliberately
keeps them off the rendered page, and says so on the page itself so the
omission doesn't get "fixed" later.
