//! Garmin render: the overview (weigh-ins as a plot and a table, the
//! devices), a page per family of daily metrics with its plot, a daily
//! log with a row per day, a page per activity, and one page of
//! records, badges, gear, workouts and goals.

pub mod activities;
pub mod daily;
pub mod parse;
pub mod plot;
pub mod records;
pub mod stamps;
// `render/render.rs` inside `render/` is the repo-wide stage layout.
#[allow(clippy::module_inception)]
pub mod render;

/// Bump when the rendered page or the grid row shape changes enough
/// that an existing `index.md` must be re-rendered.
/// v2: ids are minted through `datalib_id` under the configured source
///     and every row carries its backpointer. Every uuid moved.
pub const RENDER_VERSION: u32 = 6;
