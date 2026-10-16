# Garmin render

The render step of a `garmin` group turns the raw store into these
markdown documents under `<data_root>/<group>/render_markdown/`, with
standalone Plotly pages under `plots/`:

| file | what it shows | grid rows |
|---|---|---|
| `index.md` | **Weight**: the latest weigh-in, a plot of every weigh-in (kilograms, with body fat on a second axis when the scale reports it) and a table of the newest thirty. **Devices**: one section per registered device. **Store**: counts, and a per-metric table of days walked against days with data. | `Garmin Weight` for the page (`author` the account's full name); `Garmin Device` per device, the device name in `channel` |
| `<family>.md` | one family of per-day metrics: the latest day, a plot with one stacked panel per unit, and a table of the newest 90 days | `Garmin Metrics`, one per page |
| `days.md` | the daily log: one section per day with any value, newest first, every family's values on its own line | `Garmin Daily Log` for the page; `Garmin Day` per day, its preview leading with sleep, resting HR, HRV, steps, stress, Body Battery and readiness |
| `activities/<uuid>.md` | one activity: type, start, distance, time, pace, heart rate, calories, training effect and load, the device, whether its FIT file is held | `Garmin Activity`, the type in `channel` |
| `records.md` | personal records, badges, gear, workouts and goals, a section each | `Garmin Records` for the page; `Garmin Record`, `Garmin Badge`, `Garmin Gear`, `Garmin Workout`, `Garmin Goal` per entry |

A family page is written only when some day has a value for it. The
families, and which payload field each value comes from, are the
`FIELDS` table in `src/render/daily.rs`; heart rate, stress, Body
Battery and movement come from the daily summary, the rest from their
own endpoints. A day with several training-readiness entries shows the
one Garmin computed on waking.

Stamps: an activity's `created_at` is its local start with the offset
taken from Garmin's paired GMT and local stamps; a day's is local
midnight with the offset from that day's summary, and is empty without
one. A personal record whose `typeId` is not in the table in
`src/render/records.rs` is shown as `Record type N` with its raw value
rather than guessed at.

The minute-by-minute series (heart rate, stress, Body Battery,
respiration, steps) are mirrored but not drawn. The activities' FIT
files are read into time series (§"Time series"), not into the pages.

## Time series

The `render_time_series` step writes the same data as time series into
`<data_root>/<group>/render_time_series/time_series.doltlite_db`
(`src/render/time_series.rs`), one per metric of the account
(`docs/dev/time_series.md`):

- **Each daily field in `FIELDS`**, by calendar day (`TimeKind::Day`),
  named by its key. Values keep Garmin's unit, written in UCUM by
  `unit_of`: sleep in `s`, HRV in `ms`, intensity minutes in `min`,
  distance in `m`, heart rate as `{beats}/min`, respiration as
  `{breaths}/min`, steps as `{steps}`, floors as `{floors}`, and the
  scores (sleep, stress, Body Battery, readiness) as `{score}`. A number
  field is a `float` time series. A text field (HRV status, readiness
  level) is a `text` time series; a value of the other kind than its
  field's is left out.
- **Weight, body fat and BMI**, from the weigh-ins, by instant
  (`TimeKind::Instant`, the weigh-in's GMT stamp), in `kg`, `%` and
  `kg/m2`. Two weigh-ins at one instant keep the later.
- **Each activity's FIT file**, one time series per `record` field in
  `fit::RECORD_FIELDS` that the file holds (heart rate, cadence,
  distance, speed, altitude, position, power, running dynamics), by
  instant. `about_uuid` is the activity's page and `about_name` its
  name and date. `src/render/fit.rs` decodes the file as raw integers,
  and a scaled field becomes `(raw - offset * scale) / scale`, which
  keeps its decimals. `speed` and `altitude` are left out when the
  file has `enhanced_speed` and `enhanced_altitude`, which carry the
  same readings. A file that is held but cannot be read costs that
  activity's time series and counts as a warning on the step's Manage
  row.

A time series with no points is not written. Its id is minted from the
source id, the word `account` and the field's key, or for an
activity's, the word `activity`, the activity id and the field's name,
so it is the same on every render.

## Incrementality

Every document is in one bucket, keyed by the overview's uuid, which
declares the eight `garmin_*` tables the render reads as whole-table
inputs (`src/render/parse.rs::inputs`). When none of their rows moved
since the last render, the run is skipped for the cost of one
`dolt_log()` query (`timeseries_render`'s `skip_if_current`). Otherwise
every document is rendered again; an unchanged one writes identical
rows, which the render store records as no change, and a document not
emitted (an activity Garmin deleted) leaves the store. `RENDER_VERSION`
in `src/render/mod.rs` is the number to bump when the layout changes.

The plots load Plotly from cdn.plot.ly (pinned, with an SRI hash, in
`datalib/backend/etl/timeseries_render/src/plot.rs`); viewing them needs
network access, but the data is inlined and an offline notice replaces
the chart when the script does not load.
