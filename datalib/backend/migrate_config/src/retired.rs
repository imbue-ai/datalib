//! What the runner knew about `datalib-step` before a step named all of
//! its program's arguments, kept here for the rewrites that still read
//! that shape. The runner itself no longer knows any of it.

use serde::{Deserialize, Serialize};

/// The step program datalib ships.
pub const STEP_PROGRAM: &str = "datalib-step";

/// Whether a program word, as written, is `datalib-step`.
pub fn is_datalib_step(program: &str) -> bool {
    program == STEP_PROGRAM || program.ends_with("/datalib-step")
}

/// Whether a step's `command`, as written, runs `datalib-step`: it names
/// that program, or it is left out.
pub fn names_datalib_step(command: Option<&str>) -> bool {
    command.is_none_or(|c| c.split_whitespace().next().is_some_and(is_datalib_step))
}

/// The source types whose data is also time series. Mirrors
/// `SourceType::renders_time_series` in `datalib-step`.
pub const TIME_SERIES_SOURCE_TYPES: &[&str] = &["garmin", "yolink", "airvisual"];

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";

/// The top-level `[checkpoint_cadence]` table.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointCadence {
    pub at_most_every_secs: f64,
}

/// Which index step reads every source's step of a function, as
/// `(source function, index group, index function)`. The index step reads
/// them by pattern (`*/render_markdown`), so a source added later is read
/// without its entry being touched.
pub const FAN_INS: &[(&str, &str, &str)] = &[
    (
        "render_markdown",
        "unified_grid_and_search_index",
        "unified_grid_and_search_index",
    ),
    ("embed", "unified_grid_and_search_index", "embedding_map"),
    (
        "render_time_series",
        TIME_SERIES_INDEX_GROUP,
        "time_series_index",
    ),
];
