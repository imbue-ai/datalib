# Time series as a second destination format

**Status: written 2026-10-15. Built the same day on one branch: the
rename (§"Names"), the stores and steps with Garmin as the first
source, the applet, and the Time series card (the canvas's B and C:
a list and one chart, one axis per unit). Not built: YoLink and
AirVisual as time series, charts inside documents, and retiring
`plots/` (§"Order of work", item 5).**

## The problem

Everything a source renders today ends as a document: markdown plus
`grid_rows`, indexed by `grid_index` and qmd, served by the
`unified_index` applet. Three sources hold numbers over time rather
than text: `yolink` (sensor readings), `airvisual` (air quality) and
`garmin` (daily health metrics, weigh-ins). They share
`datalib/backend/etl/timeseries_render`, and each draws its numbers
into standalone Plotly pages under `plots/`, with every point inlined
as JSON (`datalib/backend/etl/timeseries_render/src/plot.rs:45`).

That costs three things:

1. **No query.** The numbers exist only inside HTML. Nothing can ask
   for resting heart rate between two dates, or for one sensor's last
   reading.
2. **No overlay.** A Garmin sleep score and a YoLink bedroom
   temperature are in different pages from different sources, so they
   cannot share an axis.
3. **Whole rewrites.** When any input table moved, every page and plot
   is rendered again in full; `skip_if_current`
   (`datalib/backend/etl/timeseries_render/src/page.rs:165`) only
   decides whether to skip the whole run.

The in-memory shape already exists: `Series`, one device's values for
one metric, ascending by time
(`datalib/backend/etl/timeseries_render/src/series.rs:7`). This plan
gives it a store and an index of its own.

## The decision: two destination formats, side by side

A source's render produces one or both of two formats, and each format
has its own per-source step, its own fan-in index, its own group and
its own applet. Nothing in the time-series path goes through a
document, and nothing in the document path goes through a time series.
A document that wants to show a chart names the time series by id.

What is "global" today is really global *for documents*. Each piece
gets a document name and a time-series twin:

| Today | Document format | Time-series format |
| --- | --- | --- |
| per-source `render_markdown` step | `render_markdown` (unchanged) | `render_time_series` |
| per-source store `indexed_markdown.doltlite_db` | unchanged | `time_series.doltlite_db` |
| group `unified_index` | `unified_document_index` | `unified_time_series_index` |
| fan-in step `unified_index/grid_index` | `unified_document_index/grid_index` | `unified_time_series_index/time_series_index` |
| `unified_index/qmd_aggregator`, `embedding_map`, per-source `keyword_index`, `embed` | renamed group, same functions | none |
| data-root directory `unified_index/` | `unified_document_index/` | `unified_time_series_index/` |
| applet `unified_index`, `/applet/unified_index/` | `unified_document_index` | `unified_time_series_index` |
| crate `datalib/backend/unified_index` | `unified_document_index` | `unified_time_series_index` |
| schema crate `datalib_schema` (`grid_rows`, `markdowns`, …) | unchanged | `datalib_time_series_schema` |

"Time series" is spelled out everywhere (`time_series`, never
`series`): a bare "series" is too easily read as a run of anything.

## Names: renaming the unified index

`unified_index` is hard-coded rather than read from the config: the
directory is `datalib/backend/runtime/src/layout.rs:14`, the applet id
`datalib/backend/http/src/lib.rs:816`, and the Manage labels
`datalib/backend/http/src/manage/mod.rs:500-503`. So the config, the
constants and the directory have to move together. Bringing existing
roots forward (AGENTS.md §"Keep a forward path for existing data"):

- **Config.** `datalib-migrate-config` gains a text rewrite, run by
  `datalib-http`'s config upgrade beside the qmd one
  (`datalib/backend/http/src/config_upgrade.rs`), that renames the
  group, the step ids and inputs, and the applet entry. It keeps
  comments and refuses a result that would load fewer entries.
- **Directory.** At boot, before the supervisor starts, `unified_index/`
  is renamed to `unified_document_index/` with one `rename(2)`. It is
  a derived tree and could be rebuilt, but rebuilding re-embeds every
  document, which is slow on a large root.
- **The supervisor's record.** Step ids in `system/supervisor.sqlite`
  are rewritten, so a step someone turned off stays off.
- **Applet URL.** `/applet/unified_index/…` stays as an alias, since
  card scripts people wrote call it.

## The stores

A per-source time-series store (`<source>/render_time_series/time_series.doltlite_db`)
and the index (`unified_time_series_index/time_series_index/time_series.doltlite_db`)
have the same two tables.

**`time_series`**: one row per time series.

| Column | Type | Meaning |
| --- | --- | --- |
| `ts_key` | integer, primary key | Local to the store; what points are keyed by |
| `time_series_id` | text, unique | Stable across renders and stores: a uuid from source id, entity and metric, by `docs/dev/entity_ids.md` |
| `source_id` | text | The group id |
| `entity` | text | What was measured: a device, an account, a sensor |
| `metric` | text | The provider's metric key, such as `resting_hr` |
| `label` | text | What a chart legend shows |
| `unit` | text | The unit values are stored in (§"Units and time") |
| `time_kind` | text (enum `TimeKind`) | `instant` or `day` |
| `points_hash` | text | blake3 over the points, in order; what incrementality compares |
| `point_count`, `first_t`, `last_t` | integer | For listing without reading points |

**`time_series_points`**: one row per reading.

| Column | Type | Meaning |
| --- | --- | --- |
| `ts_key` | integer | Part of the primary key |
| `t` | integer | Unix milliseconds for `instant`; days since 1970-01-01 for `day`. Part of the primary key |
| `value` | real, nullable | The reading |
| `text` | text, nullable | A reading that is not a number: a sleep stage, a door state |

**Why an integer key.** Measured (`hack/doltlite_time_series_cost/`):
with a 36-character string key a point costs about 115 bytes on disk;
with an integer key about 48, and loads and replaces take half the
time. The integer is local to its store, so the index assigns its own
and maps by `time_series_id`.

## Incrementality

The measurement settles it:

- Replacing a time series (delete its points, insert them again) in one
  transaction writes only the pages whose points changed: the same
  bytes as appending only the new points.
- But it costs time in proportion to the points replaced: about a
  second per million, 11 s for 20M points with integer keys.

So both writers replace **only the time series whose `points_hash`
changed**:

1. `render_time_series` reads the raw store, builds each time series in
   memory, and hashes its points. A time series whose hash matches the
   stored row is not written. One whose hash differs is replaced whole.
   One no longer produced is deleted. One transaction, sealed with
   `commit_run` like every writer.
2. `time_series_index` reads each input store at one commit, compares
   `points_hash` per `time_series_id` against its own rows, and
   replaces only the ones that differ. It drops the time series of a
   source that is no longer an input.

A sync that adds a day of points to 1,000 time series then costs
about the same as appending them: well under a second at 20M points.

The render can still skip the whole run when no input table moved, as
`skip_if_current` does for documents today.

## Units and time

- **One unit per quantity, chosen at render time.** Every source
  stores a quantity in the same unit, so two sources' temperatures can
  share an axis without the UI knowing either source: temperature in
  °C, volume in L, mass in kg, distance in m, every duration in s.
  These are the units YoLink's and AirVisual's metric tables already
  convert to (`MetricSpec::si_unit` and `to_si`,
  `datalib/backend/etl/timeseries_render/src/units.rs:34`). A rate or a
  score keeps its own unit (bpm, %, µg/m³). Display units (°F, lb,
  hours) are the UI's choice.
- **Two time kinds.** `instant` is a moment (a sensor reading);
  `day` is a calendar day in the person's local sense (a Garmin daily
  total), which must not be stored as midnight UTC. An interval (a
  sleep session, an activity) is not a time series; it stays a
  document.

## Serving

The `unified_time_series_index` applet opens the index read-only and
pinned, by the rules in AGENTS.md §"Doltlite". Its routes:

| Route | Does |
| --- | --- |
| `GET /time_series?q=&source_id=` | List time series: id, source, entity, metric, label, unit, kind, count, first and last `t` |
| `GET /time_series/{id}` | One time series' row |
| `POST /points` | `{ids, from, to, budget}` → per id, the points in range; when more than `budget`, downsampled into `budget` buckets of min, max and mean |

Downsampling runs in SQL (`GROUP BY` a bucket of `t`). Measured: 15 ms
for one time series of 20k points.

## Drawing

- **A chart component** in the UI draws one or more time series by id
  from `POST /points`, re-asking at a new budget on zoom. Units are
  converted for display here.
- **A time-series card** lists every time series (from
  `GET /time_series`), filterable by source and metric, and draws the
  ones picked, overlaid when they share a unit.
- **In a document.** A render writes
  `<div data-time-series="<id> <id>"></div>` where it wants a chart,
  and the document viewer draws it with the chart component. The
  backend still never parses markdown (AGENTS.md §"QMDs are
  write-only"); this is the same move as `data-handle` for chips.
  Once every time-series source draws this way, `plots/` and
  `standalone_html` go.

## Config

A source type that renders time series (`garmin`, `yolink`,
`airvisual`) gets a `render_time_series` step beside its
`render_markdown`, and the root gets a `unified_time_series_index`
group with one `time_series_index` step and the applet. New sources
get them from the wizard and `scaffold_toml`. An existing config gets
them from the config upgrade, the same way the qmd steps were added.

## Order of work

Each item is one PR, and each leaves the tree working.

1. **Rename** the unified index to the unified document index, with the
   config rewrite, directory move and URL alias (§"Names").
2. **This plan and the measurement.**
3. **Stores and steps.** `datalib_time_series_schema`, the
   `render_time_series` and `time_series_index` functions in
   `datalib-step`, `TimeKind`, the reserved directory name, Garmin as
   the first source, tests on TNG data, and the config upgrade that
   adds the steps.
4. **Serving and drawing.** The applet, the chart component and the
   time-series card.
5. **YoLink and AirVisual**, documents drawing through
   `data-time-series`, and `plots/` retired.

## Open questions

- Should a time series be findable from the document search bar, as a
  row kind, or only from the time-series card?
- Is an overlay of time series from different sources a saved view, or
  only a chart someone builds in the card each time?
- Do other sources have time series worth emitting, such as messages
  per day or email volume, or does this stay with the three
  time-series sources?
- Should the budget for downsampling come from the chart's width in
  pixels, or be a fixed number?
