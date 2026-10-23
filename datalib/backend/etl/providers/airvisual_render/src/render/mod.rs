//! AirVisual render: the whole raw store collapses into **one** markdown
//! document — a summary per device, plus a chart per physical quantity,
//! each device (and each measurement within the quantity) a series,
//! drawn from the time-series store.

pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident — see the same allow in the perseus provider.
#[allow(clippy::module_inception)]
pub mod render;
pub mod time_series;
pub mod units;

/// Bump when the rendered markdown layout or the grid
/// row shape changes enough that an existing `index.md` must be
/// re-rendered. Stamped onto the `markdowns` row AND into the render
/// cursor's `params`, so a bump invalidates the "HEAD unchanged → skip"
/// fast path too.
/// v2: ids moved to `datalib_id`'s v8 layout (none carries a stamp
///     here, but the layout changed under them).
/// v7: every stamp on a page is a `<time>`.
pub const RENDER_VERSION: u32 = 7;
