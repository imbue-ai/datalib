//! What the app knows about the configs it writes for `datalib-step`, the
//! step program datalib ships: how a step's `command` names its function,
//! and the group types and ids datalib gives meaning to. The runner knows
//! none of this (`docs/dev/plans/completed/dag_and_step_decoupling.md`).

/// The `type` of a group that renders what changed in another group's raw
/// store between two commits (`docs/dev/plans/completed/diff_renderer.md`).
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";

/// Whether `program`, a command's first word, is `datalib-step`.
pub fn is_datalib_step(program: &str) -> bool {
    program == "datalib-step" || program.ends_with("/datalib-step")
}

/// The variables the config's `[[programs]]` entry for `datalib-step`
/// passes to its steps (`pass_env`), for a `datalib-step` the server runs
/// itself. Empty when the config has no such entry or cannot be read.
pub fn datalib_step_pass_env(root: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(datalib_dag::config::root_config_path(root)) else {
        return Vec::new();
    };
    datalib_dag::config::check_text(&text)
        .cfg
        .programs
        .into_iter()
        .find(|p| is_datalib_step(p.command.trim()))
        .map(|p| p.pass_env)
        .unwrap_or_default()
}
