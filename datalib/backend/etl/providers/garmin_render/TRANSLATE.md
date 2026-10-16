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

The FIT files and the minute-by-minute series (heart rate, stress,
Body Battery, respiration, steps) are mirrored but not drawn.

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
