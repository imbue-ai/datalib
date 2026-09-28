# Garmin render

The render step of a `garmin` group turns the raw store into **one**
markdown page, `<data_root>/<group>/render_markdown/index.md`, plus one
standalone Plotly page under `plots/`:

- **Weight** — the latest weigh-in, an interactive plot of every
  weigh-in (kilograms, with body fat on a second axis when the scale
  reports it), and a table of the newest thirty.
- **Devices** — one section per registered device, each a
  `msg`-wrapped div so it has its own grid row.
- **Store** — counts: weigh-ins, activities, FIT files, listings, and
  a per-metric table of days walked against days with data.

Grid rows: one for the page (kind `Garmin Weight`, `author` the
account's full name) and one per device (kind `Garmin Device`, the
device name in `channel`).

The per-day metrics, activities and FIT files are mirrored but not
drawn.

## Incrementality

The page declares the seven `garmin_*` tables it reads as whole-table
inputs (`src/render/parse.rs::inputs`). When none of their rows moved
since the last render, the run is skipped for the cost of one
`dolt_log()` query (`timeseries_render`'s `skip_if_current`). Otherwise
the page is rendered again; an unchanged page writes identical rows,
which the render store records as no change. `RENDER_VERSION` in
`src/render/mod.rs` is the number to bump when the layout changes.

The plots load Plotly from cdn.plot.ly (pinned, with an SRI hash, in
`datalib/backend/etl/timeseries_render/src/plot.rs`); viewing them needs
network access, but the data is inlined and an offline notice replaces
the chart when the script does not load.
