//! YoLink render: the whole raw store collapses into **one** markdown
//! document — a summary of everything non-timeseries the store knows,
//! plus one interactive Plotly scatter per physical quantity, each
//! device a series on its quantity's plot.

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
/// invalidates the "HEAD unchanged → skip" fast path too. v3: `account`
/// stopped carrying the source name.
pub const RENDER_VERSION: u32 = 3;
