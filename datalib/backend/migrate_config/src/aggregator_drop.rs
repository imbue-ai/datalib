//! A config with a `qmd_aggregator` step. `grid_index` does what it did:
//! it keeps the vector cache to what a chunk still names and reports what
//! search holds of each source. The rewrite takes each built-in
//! `qmd_aggregator` out, with its comments, and an input that named one
//! (the embedding map's) reads every `embed` instead, which is what the
//! aggregator stood between. Everything else stays as written.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::retired::names_datalib_step;
use anyhow::{Context as _, Result};
use datalib_config::array::edit_string_array;
use datalib_config::order::entry_ranges;
use serde::Deserialize;

use crate::time_series_embeds::is_one_of;

const AGGREGATOR: &str = "qmd_aggregator";
const EVERY_EMBED: &str = "*/embed";

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
    fn step_id(&self) -> Option<String> {
        match (&self.group, &self.function) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }
}

/// The ids of the built-in `qmd_aggregator` steps.
fn aggregators(view: &View) -> BTreeSet<String> {
    view.steps
        .iter()
        .filter(|s| {
            names_datalib_step(s.command.as_deref()) && s.function.as_deref() == Some(AGGREGATOR)
        })
        .filter_map(StepView::step_id)
        .collect()
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(!aggregators(&view).is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let gone = aggregators(&view);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for range in entry_ranges(text) {
        if is_one_of(&text[range.clone()], &gone) {
            edits.push((range, String::new()));
        }
    }
    for inputs in view.steps.iter().filter_map(|s| s.inputs.as_ref()) {
        if !inputs.get_ref().iter().any(|i| gone.contains(i)) {
            continue;
        }
        let mut after: Vec<String> = Vec::new();
        for input in inputs.get_ref() {
            let input = match gone.contains(input) {
                true => EVERY_EMBED.to_string(),
                false => input.clone(),
            };
            if !after.contains(&input) {
                after.push(input);
            }
        }
        edits.push((
            inputs.span(),
            edit_string_array(&text[inputs.span()], &after).map_err(anyhow::Error::msg)?,
        ));
    }
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
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
function = "render_markdown"
command = "datalib-step"

[[steps]]
group = "picard"
function = "embed"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/grid_index"]

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
command = "datalib-step"
inputs = ["*/render_markdown"]

# runs after every embed
[[steps]]
group = "unified_grid_and_search_index"
function = "qmd_aggregator"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/grid_index", "*/embed"]

# the map
[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
command = "datalib-step"
inputs = ["unified_grid_and_search_index/qmd_aggregator"]
"#;

    /// The aggregator and its comment go; the map reads every embed, and
    /// still runs after the one there is; the result loads whole, and a
    /// second pass finds nothing to do.
    #[test]
    fn the_aggregator_goes_and_the_map_reads_every_embed() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains("qmd_aggregator"), "{after}");
        assert!(!after.contains("runs after every embed"), "{after}");
        assert!(after.contains("# the map"), "{after}");
        let check = crate::check_as_current(&after);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        let map = check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == "unified_grid_and_search_index/embedding_map")
            .unwrap();
        assert_eq!(map.inputs, ["picard/embed"]);
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }
}
