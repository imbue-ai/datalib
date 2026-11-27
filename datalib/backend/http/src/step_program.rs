//! What the app knows about the configs it writes for `datalib-step`, the
//! step program datalib ships: how to tell a command runs it, and the
//! group types and ids datalib gives meaning to. The runner knows
//! none of this (`datalib/backend/dag/README.md`).

/// The `type` of a group that renders what changed in another group's raw
/// store between two commits (`docs/dev/config_model.md`).
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";

/// Whether `program`, a command's first word, is `datalib-step`.
pub fn is_datalib_step(program: &str) -> bool {
    program == "datalib-step" || program.ends_with("/datalib-step")
}
