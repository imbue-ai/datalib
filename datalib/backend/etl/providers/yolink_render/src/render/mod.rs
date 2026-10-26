//! YoLink render: the whole raw store collapses into **one** markdown
//! document — a summary of everything non-timeseries the store knows,
//! plus a chart per physical quantity, each device a series on its
//! quantity's chart, drawn from the time-series store.

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
/// cursor's `params` (see [`render::cursor_params`]), so a bump
/// invalidates the "HEAD unchanged → skip" fast path too. v3: `account`
/// stopped carrying the source name. v4: ids are minted through
/// `datalib_id` under the configured source, every row carries its
/// backpointer, and a device's `upstream_id` is its name rather than
/// its kind. Every uuid moved.
/// v9: every stamp on a page is a `<time>`.
pub const RENDER_VERSION: u32 = 9;
