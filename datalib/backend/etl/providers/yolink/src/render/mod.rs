//! YoLink render: the whole raw store collapses into **one** markdown
//! document — a summary of everything non-timeseries the store knows,
//! plus one interactive Plotly scatter per physical quantity, each
//! device a series on its quantity's plot.
//!
//! Layout under `<data_root>/<stanza>/rendered_md/`:
//!
//! ```text
//! index.md                  the document
//! (its rows go into the source's render store: 1 doc row + 1 per device)
//! plots/temperature.html    standalone Plotly page, iframed from index.md
//! plots/humidity.html
//! plots/volume.html
//! _render_cursor.json       written by datalib_etl::render_cursor
//! ```
//!
//! ## Why the cursor is just "the store's HEAD"
//!
//! Every other doltlite-backed provider buckets its raw rows into many
//! documents and so runs a `dolt_diff_<table>` scan to find the changed
//! buckets. YoLink has exactly one bucket — the whole database is one
//! page — so there is nothing for a diff to narrow: if *anything* was
//! appended, the single page is stale, and if nothing was, it isn't.
//! [`parse::parse`] therefore compares the store's HEAD commit hash
//! against `_render_cursor.json` and returns [`parse::Parsed::UpToDate`]
//! without reading a single reading row. See [`parse`] for the cost
//! argument.
//!
//! ## Units
//!
//! Series are grouped by *physical quantity*, not by metric name, and
//! every value is converted to SI on the way into the plot — so a
//! device reporting °F and one reporting °C land on the same axis in
//! °C. The conversion policy is the single table in [`units`].

pub mod parse;
pub mod plot;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident — see the same allow in the perseus provider.
#[allow(clippy::module_inception)]
pub mod render;
pub mod units;

/// Bump when the rendered markdown layout, the plot HTML, or the grid
/// row shape changes enough that an existing `index.md` must be
/// re-rendered. Stamped onto the `markdowns` row AND into the render
/// cursor's `params` (see [`render::cursor_params`]), so a bump
/// invalidates the "HEAD unchanged → skip" fast path too.
///
/// v1 — initial timeseries renderer.
/// v2 — samples are joined by a line (`lines+markers`) instead of
///      standing alone as markers.
pub const RENDER_VERSION: u32 = 2;
