//! AirVisual render: the whole raw store collapses into **one** markdown
//! document — a summary per device, plus a chart per physical quantity,
//! each device (and each measurement within the quantity) a series,
//! drawn from the time-series store.

pub mod parse;
// `render/render.rs` inside `render/` is the layout every provider's
// render crate shares, not an accident. Allowed here rather than
// repo-wide so a module named like its parent anywhere else still fails
// clippy's `module_inception`.
#[allow(clippy::module_inception)]
pub mod render;
pub mod time_series;
pub mod units;

/// Bump when the rendered markdown layout or the grid
/// row shape changes enough that an existing page must be
/// re-rendered. Stamped onto the `markdowns` row AND into the render
/// cursor's `params`, so a bump invalidates the "HEAD unchanged → skip"
/// fast path too.
pub const RENDER_VERSION: u32 = 9;
