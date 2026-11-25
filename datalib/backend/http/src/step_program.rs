//! What the app knows about the configs it writes for `datalib-step`, the
//! step program datalib ships: how a step's `command` names its function,
//! and the group types and ids datalib gives meaning to. The runner knows
//! none of this (`datalib/backend/dag/README.md`).

/// The `type` of a group that renders what changed in another group's raw
/// store between two commits (`docs/dev/plans/completed/diff_renderer.md`).
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The group the time-series index is filed under, and the tree it writes.
pub const TIME_SERIES_INDEX_GROUP: &str = "unified_time_series_index";

/// Whether `program`, a command's first word, is `datalib-step`.
pub fn is_datalib_step(program: &str) -> bool {
    program == "datalib-step" || program.ends_with("/datalib-step")
}

/// The function a `datalib-step run <function> …` command performs; `None`
/// for any other command.
pub fn datalib_step_function(command: &str) -> Option<String> {
    let argv = shlex::split(command)?;
    match argv.as_slice() {
        [program, run, function, ..] if is_datalib_step(program) && run == "run" => {
            Some(function.clone())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_is_read_off_a_datalib_step_command_and_nothing_else() {
        assert_eq!(
            datalib_step_function("datalib-step run render_markdown slack").as_deref(),
            Some("render_markdown")
        );
        assert_eq!(
            datalib_step_function("/opt/bin/datalib-step run unified_grid_and_search_index")
                .as_deref(),
            Some("unified_grid_and_search_index")
        );
        assert_eq!(datalib_step_function("datalib-step help"), None);
        assert_eq!(datalib_step_function("./bin/fetch run ingest"), None);
    }
}
