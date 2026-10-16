# Time series

A source whose data is numbers over time (Garmin's daily metrics and
weigh-ins) renders it twice: as documents, by `render_markdown`, and as
time series, by `render_time_series`. This doc covers the time-series
path: the stores, how points are encoded, units, the index and the
applet. What is planned but not built is in
[`plans/time_series.md`](plans/time_series.md).

## The path

```
<source>/ingest  ──render_time_series──▶  <source>/render_time_series/time_series.doltlite_db
                                              rows + points, as chunks
                                                    │
                                    time_series_index (reads each at one commit)
                                                    ▼
             unified_time_series_index/time_series_index/time_series.doltlite_db
                                              rows + where each source's store is, no points
                                                    │
                       unified_time_series_index applet ──▶ Time series card
```

- **`render_time_series`** (`datalib/backend/datalib_step/src/time_series.rs`)
  reads the source's raw store, builds every time series in memory and
  hands them to `datalib_etl_time_series::apply`, which writes only what
  differs.
- **`time_series_index`** (same file) reads each source store named in
  its `inputs` at that store's newest commit. It copies the store's
  rows and records the store's path and that commit. It copies no
  points.
- **The applet** (`datalib/backend/applets/src/unified_time_series_index.rs`)
  lists from the index, and reads points from each source's store at
  the commit the index recorded for it.

## A source's store

Two tables (`datalib/backend/time_series_schema/src/`).

**`time_series`**, one row per time series:

| Column | Meaning |
| --- | --- |
| `source_id`, `ts_key` | The primary key. `ts_key` is local to the source's store and keys its chunks |
| `time_series_id` | Stable across renders: minted by the provider from the source id and `name` (`docs/dev/entity_ids.md`) |
| `about_uuid` | The document the time series belongs to (an activity), or null for one about the source as a whole |
| `name` | The provider's key for what is measured, such as `resting_hr`; unique within the source and `about_uuid` |
| `human_name` | What a chart legend shows, such as `Resting HR` |
| `unit` | A UCUM code (§"Units") |
| `value_type` | `integer`, `float` or `text` (`ValueType`) |
| `time_kind` | `instant`: `t` is unix milliseconds. `day`: `t` is days since 1970-01-01, a calendar day as the person lived it, never stored as midnight UTC (`TimeKind`) |
| `point_count`, `first_t`, `last_t` | For listing without reading points |

**`time_series_chunks`**, one row per run of up to 1,024 points of one
time series:

| Column | Meaning |
| --- | --- |
| `ts_key`, `first_t` | The primary key, so one time series' chunks sit together, in time order |
| `last_t`, `n` | The last `t` in the chunk and how many points it holds |
| `min`, `max`, `sum` | Over the values as numbers; null for text. A bucketed read uses them in place of decoding a chunk that lies inside one bucket |
| `encoding` | Which layout `points` is in; a reader refuses one it does not know |
| `points` | The encoded points (§"The chunk encoding") |

A time series is cut into chunks by position from its first point. An
append therefore changes only the last chunk, and a rewrite of the
series writes only the chunks whose bytes changed.

## The chunk encoding

`datalib/backend/etl/time_series/src/chunk.rs`, encoding 1. A blob is
one byte saying whether the rest is deflated, then:

1. **The count**, as a varint.
2. **The times:** the first `t` and the first gap as zigzag varints,
   then each gap's change from the one before, bit-packed. Evenly
   spaced times pack to a single byte, however many there are.
3. **The values**, after a tag byte saying which of these follows:
   - **Integers:** the first value, then each value's change from the
     one before, bit-packed.
   - **Floats with few decimals:** the fewest decimals `e` (up to 15)
     for which every value times 10^e is an integer that reads back to
     the same bits. Those integers are then stored as integers are, so
     a reading with two decimals costs what an integer costs.
   - **Other floats:** 8 bytes each, little-endian. NaN, infinities
     and -0.0 come here too, so they read back bit for bit.
   - **Text:** a dictionary of the distinct texts in order of first
     appearance, then each point's index into it, bit-packed.

"Bit-packed" means one byte giving the narrowest bit width that holds
every number in the run, then the numbers at that width, least
significant bit first. The body is deflated when that makes it smaller.
Every layout reads back exactly what was written; the tests in
`chunk.rs` check that, and check that a cut-off or corrupt blob is an
error rather than a panic.

Measured on one person's 13 Garmin activities, 257,694 values at one a
second, the store takes 0.93 bytes a value on disk, against 5.32 for
the FIT files themselves. Latitude and longitude are the costly part:
in degrees they have no short decimal form. The measurement, the other
chunk sizes tried, and how to run it are in
`datalib/backend/etl/time_series/measure/README.md`.

A render computing a float should take an offset off before dividing.
For an altitude stored as 2501 with scale 5 and offset 500,
`(2501 - 2500) / 5` is 0.2, but `2501 / 5 - 500` is
0.19999999999998863, which has no short decimal form and costs 8 bytes.

## Units

A unit is a code in UCUM, the Unified Code for Units of Measure
(<https://ucum.org/ucum>), which HL7 FHIR and OpenTelemetry also use.
Some examples:

| What | UCUM |
| --- | --- |
| seconds, metres, kilograms | `s`, `m`, `kg` |
| milliseconds, minutes | `ms`, `min` |
| acceleration | `m/s2` |
| BMI | `kg/m2` |
| heart rate | `{beats}/min` |
| steps, a score | `{steps}`, `{score}` |
| °C, °F | `Cel`, `[degF]` |
| percent | `%` |
| a pure number | `1` |

`Unit::parse` (`datalib/backend/time_series_schema/src/unit.rs`) reads
the whole grammar: prefixes, `.` and `/`, integer exponents,
parentheses, `{annotations}` and integer factors. It knows only the
atoms in its `ATOMS` table; add one there when a source needs it.
`apply` refuses a time series whose unit does not parse.

A parsed unit gives:

- **`quantity()`:** the powers of the seven base quantities, then the
  annotations, such as `L1.T-2` or `T-1{beats}`. Two time series can
  share a chart axis when their quantities are equal. UCUM gives an
  annotation no meaning, but we keep units that differ only in one
  apart, so steps and a score never share an axis.
- **`factor()` and `offset()`:** a value times the factor, plus the
  offset, is in the coherent SI unit of its quantity (kg, not g). The
  offset is non-zero only for °C and °F, which UCUM allows only alone.
- **`label()`:** what an axis prints: `m/s²`, `beats/min`, `°C`.

A source stores values in the unit it gets them in (Garmin's HRV in
`ms`, its intensity minutes in `min`). The reader converts between
units of one quantity.

## The index

The index holds the same `time_series` table as a source's store, with
every source's rows, and one more:

**`time_series_sources`**: `source_id`, `store` (the store's path under
the data root, `/`-separated), and `commit_hash`, the commit of that
store the rows were read at.

A reader opens each source's store at that commit, with a detached
read-only open (`doltlite_raw::open_reader(path, Some(commit))`). So:

- **One index commit is one consistent view.** A render writing its
  store's next commit changes nothing a reader of the index sees until
  `time_series_index` runs again and records the new commit.
- **The index stays small.** It holds one row per time series and one
  per source, so indexing a source costs reading its rows, not its
  points.
- **A source store can change under the index.** A store that was
  deleted no longer has the recorded commit, and one rebuilt in a new
  shape has it only in the old shape, which this build's reads fail on.
  The applet then reports that source's time series as unreadable until
  the index runs again.

A source named in `inputs` with no store yet keeps whatever the index
held for it. A source no longer in `inputs` is dropped, rows and record.

## The applet

`datalib-applet unified_time_series_index`, behind the gateway at
`/applet/unified_time_series_index/`:

| Route | Answers |
| --- | --- |
| `GET /time_series?source_id=&q=` | Every time series' row, with its unit read: `unit_label`, `quantity`, `unit_factor`, `unit_offset` |
| `GET /time_series/{id}` | One |
| `POST /points` `{ids, from, to, budget}` | Per id, the points in `[from, to]`, or past `budget` points, at most `budget` buckets of min, max and mean. Ids the index lacks come back in `missing`, ones whose store cannot be read in `unreadable`, with the reason |

`from` and `to` are in each time series' own `t`. The bucketing is
`read::answer` (`datalib/backend/etl/time_series/src/read.rs`), a pure
function over the chunks that overlap the range.

The Time series card (`docs/dev/cards.md`, `timeSeriesView()`) draws
what the applet answers. It converts every value to SI units with
`unit_factor` and `unit_offset`, gives each quantity its own axis (two
at most), and prints each axis in a unit that suits it.

## Rebuilding

Both stores are derived. Each records its shape in `_datalib_meta`.
When the shape changes, the launch's migrate pass asks for the step to
run again (`datalib/backend/datalib_step/src/migrate.rs`), and the
step's `store::open` drops each table whose shape changed and creates
it empty, in the same file, before the step fills it again. Because the
index has every `render_time_series` step as an input, it runs after
them and records their new commits.
