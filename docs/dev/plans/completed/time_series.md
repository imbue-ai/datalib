# Time series as a second destination format

**Status: written 2026-10-15. Built: the rename (§"Names"), the
stores and steps with Garmin as the first source, the applet and the
Time series card. On 2026-10-16 the stores changed from one row per
point to encoded chunks, the index stopped copying points, and units
became UCUM codes. What is built is described in
[`../../time_series.md`](../../time_series.md), which replaces this plan's
sections on the stores, units and serving; §"Names" and §"Config" are
kept as the record of what was decided. Every item of §"Order of
work" is built, the last ones on 2026-10-16: YoLink and AirVisual as
time series, charts inside documents with the Plotly `plots/` pages
retired, and time series from Garmin's FIT files. What is left is
§"Open questions".**

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

## The stores, units and serving

Built, and described in [`../../time_series.md`](../../time_series.md). The
first version kept one row per point and replaced a time series whole
when a hash of its points moved. Measured
(`hack/doltlite_time_series_cost/`): a point cost about 48 bytes on
disk with an integer key, and replacing cost about a second per
million points. Encoded chunks cost under one byte a value
(`datalib/backend/etl/time_series/measure/README.md`), and a write
replaces only the chunks whose bytes changed.

## Drawing

- **A chart component** in the UI draws one or more time series by id
  from `POST /points`, re-asking at a new budget on zoom. Units are
  converted for display here.
- **A time-series card** lists every time series (from
  `GET /time_series`), filterable by source and name, and draws the
  ones picked, overlaid when they measure the same quantity. Built.
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
6. **Time series from FIT files**: one per record field per activity,
   with `about_uuid` naming the activity's document, decoded from the
   FIT files Garmin's ingest already keeps.

All six are built.

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
