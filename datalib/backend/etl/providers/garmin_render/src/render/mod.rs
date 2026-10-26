//! Garmin render: the overview (weigh-ins as a chart and a table, the
//! devices), a page per family of daily metrics with its charts, a
//! daily log with a row per day, a page per activity with a chart of
//! its heart rate and speed, and one page of records, badges, gear,
//! workouts and goals. A chart is a `data-time-series` element the
//! document view draws from the time-series store.

pub mod activities;
pub mod daily;
pub mod fit;
pub mod parse;
pub mod records;
pub mod stamps;
pub mod time_series;
// `render/render.rs` inside `render/` is the repo-wide stage layout.
#[allow(clippy::module_inception)]
pub mod render;

/// Bump when the rendered page or the grid row shape changes enough
/// that an existing `index.md` must be re-rendered.
/// v2: ids are minted through `datalib_id` under the configured source
///     and every row carries its backpointer. Every uuid moved.
/// v8: every stamp on a page is a `<time>`.
pub const RENDER_VERSION: u32 = 8;
