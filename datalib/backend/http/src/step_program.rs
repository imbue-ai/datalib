//! The group types and ids of Datalib's own configs that the Manage
//! screen gives meaning to: a comparison's group, and the group the
//! time-series index is filed under. The runner knows none of this
//! (`datalib/backend/dag/README.md`).

/// The `type` of a group that renders what changed in another group's raw
/// store between two commits (`docs/dev/plans/completed/diff_renderer.md`).
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";
