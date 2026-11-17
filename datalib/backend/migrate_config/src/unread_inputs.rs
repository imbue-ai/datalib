//! A config whose `grid_index` still names `render_time_series` or
//! `render_occurrences` steps in its `inputs`, as a pattern
//! (`*/render_time_series`) or by id. They were wired in so `grid_index`
//! could copy their problem rows; problems are now read in each step's
//! own store, and `grid_index` passes over those inputs
//! (`render_inputs` in `datalib_step/src/grid_index.rs`). All they still
//! did was run `grid_index` again after every such render. The rewrite
//! takes them out of `grid_index`'s `inputs`, and changes nothing else.

use anyhow::{Context as _, Result};
use datalib_dag::config::names_datalib_step;
use datalib_dag::config_array::edit_string_array;
use serde::Deserialize;

use crate::index_name::NEW as UNIFIED_GRID_AND_SEARCH_INDEX_DIR;

/// The functions `grid_index` no longer reads; the same list
/// `render_inputs` passes over.
const UNREAD: &[&str] = &["render_time_series", "render_occurrences"];
const GRID_INDEX: &str = "grid_index";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    steps: Vec<StepView>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
}

impl StepView {
    fn is_grid_index(&self) -> bool {
        let id = match (&self.group, &self.function) {
            (Some(g), Some(f)) => format!("{g}/{f}"),
            _ => self.id.clone().unwrap_or_default(),
        };
        id == format!("{UNIFIED_GRID_AND_SEARCH_INDEX_DIR}/{GRID_INDEX}")
            && names_datalib_step(self.command.as_deref())
    }
}

fn unread(input: &str) -> bool {
    input
        .rsplit_once('/')
        .is_some_and(|(_, function)| UNREAD.contains(&function))
}

/// `grid_index`'s `inputs` and what is left of them, when any goes.
fn edit(view: &View) -> Option<(&toml::Spanned<Vec<String>>, Vec<String>)> {
    let inputs = view
        .steps
        .iter()
        .find(|s| s.is_grid_index())?
        .inputs
        .as_ref()?;
    let kept: Vec<String> = inputs
        .get_ref()
        .iter()
        .filter(|i| !unread(i))
        .cloned()
        .collect();
    (kept.len() != inputs.get_ref().len()).then_some((inputs, kept))
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(edit(&view).is_some())
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let Some((inputs, kept)) = edit(&view) else {
        return Ok(text.to_string());
    };
    let mut out = text.to_string();
    out.replace_range(
        inputs.span(),
        &edit_string_array(&text[inputs.span()], &kept).map_err(anyhow::Error::msg)?,
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"# the away team's watches and calendars
[[groups]]
id = "riker"
type = "garmin"

[[steps]]
group = "riker"
function = "ingest"
command = "datalib-step"

[[steps]]
group = "riker"
function = "render_markdown"
command = "datalib-step"
inputs = ["riker/ingest"]

[[steps]]
group = "riker"
function = "render_time_series"
command = "datalib-step"
inputs = ["riker/ingest"]

[[groups]]
id = "duty-roster"
type = "calendar"

[[steps]]
group = "duty-roster"
function = "ingest"
command = "datalib-step"
[steps.params.ics]
path = "/deck8/roster.ics"

[[steps]]
group = "duty-roster"
function = "render_markdown"
command = "datalib-step"
inputs = ["duty-roster/ingest"]

[[steps]]
group = "duty-roster"
function = "render_occurrences"
command = "datalib-step"
inputs = ["duty-roster/ingest"]

[[groups]]
id = "unified_grid_and_search_index"

# every source's documents
[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
command = "datalib-step"
inputs = [
    "*/render_markdown", # the logs
    "*/render_time_series",
    "duty-roster/render_occurrences",
]
params.not_searched = ["riker"]

[[groups]]
id = "unified_time_series_index"

[[steps]]
group = "unified_time_series_index"
function = "time_series_index"
command = "datalib-step"
inputs = ["*/render_time_series"]
"#;

    fn check(text: &str) -> datalib_dag::config::ConfigCheck {
        let check = datalib_dag::config::check_text(text);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        check
    }

    fn inputs_of(check: &datalib_dag::config::ConfigCheck, id: &str) -> Vec<String> {
        let step = check.cfg.steps.iter().find(|s| s.id == id).unwrap();
        step.inputs.clone()
    }

    /// The time-series and occurrences inputs leave `grid_index`, whether
    /// written as a pattern or by id; its other input keeps its comment,
    /// every other line of the file is as it was, the time-series index
    /// still reads every time series, and a second pass finds nothing to do.
    #[test]
    fn grid_index_stops_naming_the_steps_it_does_not_read() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        let grid = "unified_grid_and_search_index/grid_index";
        let ts_index = "unified_time_series_index/time_series_index";
        let check = check(&after);
        assert_eq!(
            inputs_of(&check, grid),
            ["riker/render_markdown", "duty-roster/render_markdown"]
        );
        assert_eq!(inputs_of(&check, ts_index), ["riker/render_time_series"]);
        assert_eq!(
            after,
            BEFORE.replace(
                "    \"*/render_time_series\",\n    \"duty-roster/render_occurrences\",\n",
                ""
            ),
        );
        assert!(
            after.contains("\"*/render_markdown\", # the logs"),
            "{after}"
        );
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// A `grid_index` that is someone's own program reads what it reads,
    /// and a config with nothing to take out is returned as it was.
    #[test]
    fn a_custom_grid_index_and_a_current_config_are_left_alone() {
        let custom = BEFORE.replace(
            "function = \"grid_index\"\ncommand = \"datalib-step\"\n",
            "function = \"grid_index\"\ncommand = \"my-grid\"\n",
        );
        assert!(!is_retired(&custom).unwrap());
        assert_eq!(rewrite(&custom).unwrap(), custom);
        let current = rewrite(BEFORE).unwrap();
        assert_eq!(rewrite(&current).unwrap(), current);
    }
}
