//! Garmin render: the raw store collapses into **one** markdown page —
//! the weigh-in history as an interactive plot and a table, plus the
//! account's devices. The rest of the store (per-day metrics,
//! activities, FIT files) is mirrored but not yet rendered.

pub mod parse;
pub mod plot;
// `render/render.rs` inside `render/` is the repo-wide stage layout.
#[allow(clippy::module_inception)]
pub mod render;

/// Bump when the rendered page or the grid row shape changes enough
/// that an existing `index.md` must be re-rendered.
/// v2: ids are minted through `datalib_id` under the configured source
///     and every row carries its backpointer. Every uuid moved.
pub const RENDER_VERSION: u32 = 2;
