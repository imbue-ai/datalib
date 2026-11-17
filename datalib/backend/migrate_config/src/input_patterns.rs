//! A config whose index steps name every source's step by id. A source
//! added by hand and not added to those lists is left out of the grid or
//! out of search, with nothing on screen to say why. Where an index step
//! names every step of a function it reads, the rewrite puts the pattern
//! (`*/render_markdown`) in place of those ids, which reads the same steps
//! today and every one added later. A list that leaves a step out is left
//! as written: whoever wrote it may have meant it.

use anyhow::{Context as _, Result};
use datalib_dag::config::FAN_INS;
use datalib_dag::config_array::edit_string_array;
use datalib_dag::input_pattern::{names, WILDCARD_GROUP};
use serde::Deserialize;

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
    inputs: Option<toml::Spanned<Vec<String>>>,
}

impl StepView {
    fn step_id(&self) -> Option<String> {
        match (&self.group, &self.function) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }
}

/// `inputs` with the ids of every step of each function in `functions`
/// replaced by that function's pattern, where `inputs` names all of them;
/// `None` when nothing changes.
fn with_patterns(
    own: &str,
    inputs: &[String],
    functions: &[&str],
    ids: &[String],
) -> Option<Vec<String>> {
    let mut out = inputs.to_vec();
    for function in functions {
        let pattern = format!("{WILDCARD_GROUP}/{function}");
        if out.contains(&pattern) {
            continue;
        }
        let of_function: Vec<&String> = ids
            .iter()
            .filter(|id| id.as_str() != own && names(&pattern, id))
            .collect();
        if !of_function.iter().all(|id| out.contains(id)) {
            continue;
        }
        let at = out
            .iter()
            .position(|i| of_function.contains(&i))
            .unwrap_or(out.len());
        out.retain(|i| !of_function.contains(&i));
        out.insert(at.min(out.len()), pattern);
    }
    (out != inputs).then_some(out)
}

/// Each index step's `inputs` span and what to write there.
fn edits(view: &View) -> Vec<(std::ops::Range<usize>, Vec<String>)> {
    let ids: Vec<String> = view.steps.iter().filter_map(StepView::step_id).collect();
    let mut out = Vec::new();
    for step in &view.steps {
        let (Some(group), Some(function), Some(inputs)) =
            (&step.group, &step.function, &step.inputs)
        else {
            continue;
        };
        let reads: Vec<&str> = FAN_INS
            .iter()
            .filter(|&&(_, g, i)| g == group && i == function)
            .map(|&(f, _, _)| f)
            .collect();
        if reads.is_empty() {
            continue;
        }
        let own = format!("{group}/{function}");
        if let Some(after) = with_patterns(&own, inputs.get_ref(), &reads, &ids) {
            out.push((inputs.span(), after));
        }
    }
    out
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(!edits(&view).is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let mut changes = edits(&view);
    changes.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut out = text.to_string();
    for (span, after) in changes {
        let written = edit_string_array(&text[span.clone()], &after).map_err(anyhow::Error::msg)?;
        out.replace_range(span, &written);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "picard"
type = "email"

[[steps]]
group = "picard"
function = "ingest"
command = "datalib-step"

[[steps]]
group = "picard"
function = "render_markdown"
command = "datalib-step"
inputs = ["picard/ingest"]

[[steps]]
group = "picard"
function = "keyword_index"
command = "datalib-step"
inputs = ["picard/render_markdown"]

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
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
command = "datalib-step"
inputs = [
    "picard/render_markdown", # the captain's mail
    "riker/render_markdown",
    "riker/render_time_series",
]

[[steps]]
group = "unified_grid_and_search_index"
function = "qmd_aggregator"
command = "datalib-step"
inputs = ["picard/keyword_index"]

[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/qmd_aggregator"]
"#;

    fn loaded_inputs(text: &str, step: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(text);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == step)
            .unwrap()
            .inputs
            .clone()
    }

    fn written_inputs(text: &str, step: &str) -> Vec<String> {
        let view: View = toml::from_str(text).unwrap();
        view.steps
            .iter()
            .find(|s| s.step_id().as_deref() == Some(step))
            .unwrap()
            .inputs
            .as_ref()
            .unwrap()
            .get_ref()
            .clone()
    }

    /// Every list that names every step of its function becomes the
    /// pattern; the steps each index reads are the same before and after,
    /// a step that is not an index is left alone, and a second pass finds
    /// nothing to do.
    #[test]
    fn complete_lists_become_patterns_and_read_the_same_steps() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        let grid = "unified_grid_and_search_index/grid_index";
        let aggregator = "unified_grid_and_search_index/qmd_aggregator";
        let map = "unified_grid_and_search_index/embedding_map";
        assert_eq!(
            written_inputs(&after, grid),
            [
                "*/render_markdown",
                "*/render_time_series",
                "*/render_occurrences"
            ]
        );
        assert_eq!(
            written_inputs(&after, aggregator),
            ["*/keyword_index", "*/embed"]
        );
        assert_eq!(written_inputs(&after, map), written_inputs(BEFORE, map));
        for step in [grid, aggregator, map] {
            assert_eq!(
                loaded_inputs(&after, step),
                loaded_inputs(BEFORE, step),
                "{step}"
            );
        }
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// A list that leaves a source out keeps naming the ones it names: the
    /// rewrite cannot tell an omission from a choice, and a pattern would
    /// read the one left out.
    #[test]
    fn a_list_that_leaves_a_step_out_is_left_as_written() {
        let partial = BEFORE.replace("    \"riker/render_markdown\",\n", "");
        let after = rewrite(&partial).unwrap();
        assert_eq!(
            written_inputs(&after, "unified_grid_and_search_index/grid_index"),
            [
                "picard/render_markdown",
                "*/render_time_series",
                "*/render_occurrences"
            ]
        );
    }
}
