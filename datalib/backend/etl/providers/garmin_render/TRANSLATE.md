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

This is the proof-of-concept render. The per-day metrics, activities
and FIT files are mirrored but not yet drawn; the obvious next pages
are a per-day dashboard (sleep, HRV, resting heart rate, body battery
over time — the same plot machinery, different series) and one page
per activity.

## Incrementality

One page, so incrementality is the raw store's HEAD: an ingest that
appended nothing leaves HEAD where it was and the render is a no-op.
The document's `source_fingerprint` hashes what the page reads — the
account name, every weigh-in's values, every device, the per-metric
counts — so the index reloads it only when something it shows moved.
`RENDER_VERSION` in `src/render/mod.rs` is the number to bump when the
layout changes.

The plots load Plotly from cdn.plot.ly (pinned, with an SRI hash);
viewing them needs network access, but the data is inlined and an
offline notice replaces the chart when the script does not load.
