//! A config whose `grid_index` does not read its sources'
//! `render_time_series` and `render_occurrences` steps. Those steps keep
//! problem rows (an unreadable file, an event whose occurrences could
//! not be listed) in their own stores, and `grid_index` copies them into
//! the problems list the Manage counts are made from; a step it does not
//! read reaches neither. The rewrite adds each such step of datalib's
//! own to `grid_index`'s `inputs`, and changes nothing else.

use crate::document_index::NEW as UNIFIED_DOCUMENT_INDEX_DIR;
use anyhow::{Context as _, Result};
use datalib_dag::config::names_datalib_step;
use datalib_dag::config_array::edit_string_array;
use serde::Deserialize;

/// The functions whose problem rows `grid_index` copies.
const READ_FOR_PROBLEMS: &[&str] = &["render_time_series", "render_occurrences"];
const GRID_INDEX: &str = "grid_index";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    steps: Vec<StepView>,
}

#[derive(Deserialize)]
struct StepView {
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
    fn is(&self, group: &str, function: &str) -> bool {
        self.group.as_deref() == Some(group) && self.function.as_deref() == Some(function)
    }
}

/// The ids `grid_index` should read and does not, and its `inputs`.
fn missing(view: &View) -> Option<(Vec<String>, &toml::Spanned<Vec<String>>)> {
    let inputs = view
        .steps
        .iter()
        .find(|s| s.is(UNIFIED_DOCUMENT_INDEX_DIR, GRID_INDEX))?
        .inputs
        .as_ref()?;
    let ids: Vec<String> = view
        .steps
        .iter()
        .filter(|s| {
            names_datalib_step(s.command.as_deref())
                && s.function
                    .as_deref()
                    .is_some_and(|f| READ_FOR_PROBLEMS.contains(&f))
        })
        .filter_map(|s| {
            Some(format!(
                "{}/{}",
                s.group.as_deref()?,
                s.function.as_deref()?
            ))
        })
        .filter(|id| !inputs.get_ref().contains(id))
        .collect();
    (!ids.is_empty()).then_some((ids, inputs))
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(missing(&view).is_some())
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let Some((ids, inputs)) = missing(&view) else {
        return Ok(text.to_string());
    };
    let mut after = inputs.get_ref().clone();
    after.extend(ids);
    let mut out = text.to_string();
    out.replace_range(
        inputs.span(),
        &edit_string_array(&text[inputs.span()], &after).map_err(anyhow::Error::msg)?,
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

[[steps]]
group = "riker"
function = "render_markdown"
inputs = ["riker/ingest"]

[[steps]]
group = "riker"
function = "render_time_series"
inputs = ["riker/ingest"]

[[groups]]
id = "duty-roster"
type = "calendar"

[[steps]]
group = "duty-roster"
function = "ingest"
[steps.params.ics]
path = "/deck8/roster.ics"

[[steps]]
group = "duty-roster"
function = "render_markdown"
inputs = ["duty-roster/ingest"]

[[steps]]
group = "duty-roster"
function = "render_occurrences"
inputs = ["duty-roster/ingest"]

[[groups]]
id = "unified_document_index"

# every source's documents
[[steps]]
group = "unified_document_index"
function = "grid_index"
inputs = [
    "riker/render_markdown",
    "duty-roster/render_markdown",
]
"#;

    fn grid_inputs(text: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(text);
        assert_eq!(
            check.dropped(),
            0,
            "{}",
            check.render("config.toml".as_ref())
        );
        check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == "unified_document_index/grid_index")
            .unwrap()
            .inputs
            .clone()
    }

    /// Each source's time-series and occurrences steps join
    /// `grid_index`'s inputs; every comment and entry stays where it was,
    /// and a second pass finds nothing to do.
    #[test]
    fn grid_index_reads_the_steps_that_keep_problems_of_their_own() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert_eq!(
            grid_inputs(&after),
            [
                "riker/render_markdown",
                "duty-roster/render_markdown",
                "riker/render_time_series",
                "duty-roster/render_occurrences",
            ]
        );
        assert!(after.contains("# every source's documents"), "{after}");
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// A package's own program keeps whatever it keeps; only datalib's
    /// steps are wired, and a config with no `grid_index` is left alone.
    #[test]
    fn a_package_step_and_a_config_without_the_index_are_left_alone() {
        let package = BEFORE.replace(
            "function = \"render_time_series\"\n",
            "function = \"render_time_series\"\ncommand = \"weather-ts\"\n",
        );
        let after = rewrite(&package).unwrap();
        assert!(!grid_inputs(&after).contains(&"riker/render_time_series".to_string()));
        let no_index = BEFORE
            .split("[[groups]]\nid = \"unified_document_index\"")
            .next()
            .unwrap();
        assert!(!is_retired(no_index).unwrap());
    }
}
