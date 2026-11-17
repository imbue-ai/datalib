//! A config whose sources each have a `keyword_index` step. `grid_index`
//! does that work now: it puts every source it reads in the keyword
//! search, but those its `params.not_searched` lists. The rewrite takes
//! each built-in `keyword_index` out, with its comments; lists in
//! `not_searched` each source `grid_index` reads that had none, so
//! nothing that was out of search comes into it; and points every input
//! that named a keyword step (an `embed`, the embedding map) at `grid_index`
//! instead. A config with no `grid_index` gets one, reading every
//! source's render. Everything else stays as written.

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{bail, Context as _, Result};
use datalib_dag::config::names_datalib_step;
use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::entry_ranges;
use datalib_dag::input_pattern::names;
use serde::Deserialize;

use crate::index_name::{NEW as INDEX_GROUP, OLD as OLD_INDEX_GROUPS};
use crate::time_series_embeds::is_one_of;

const KEYWORD_INDEX: &str = "keyword_index";
const GRID_INDEX: &str = "grid_index";
const RENDER: &str = "render_markdown";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    groups: Vec<GroupView>,
    #[serde(default)]
    steps: Vec<StepView>,
}

#[derive(Deserialize)]
struct GroupView {
    id: String,
}

/// What a `grid_index` the rewrite adds reads.
const GRID_READS: &[&str] = &["*/render_markdown"];

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
    #[serde(default)]
    params: Option<toml::Value>,
}

impl StepView {
    fn step_id(&self) -> Option<String> {
        match (&self.group, &self.function) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }

    fn is(&self, group: &str, function: &str) -> bool {
        self.group.as_deref() == Some(group) && self.function.as_deref() == Some(function)
    }
}

/// The ids of the built-in `keyword_index` steps.
fn keyword_steps(view: &View) -> BTreeSet<String> {
    view.steps
        .iter()
        .filter(|s| {
            names_datalib_step(s.command.as_deref()) && s.function.as_deref() == Some(KEYWORD_INDEX)
        })
        .filter_map(StepView::step_id)
        .collect()
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(!keyword_steps(&view).is_empty())
}

/// The sources `grid` reads a render of that had no keyword step, sorted.
fn not_searched(view: &View, read: &[String], gone: &BTreeSet<String>) -> Vec<String> {
    let searched: BTreeSet<&str> = gone
        .iter()
        .filter_map(|id| id.split_once('/').map(|(g, _)| g))
        .collect();
    let mut out: Vec<String> = view
        .steps
        .iter()
        .filter(|s| s.function.as_deref() == Some(RENDER))
        .filter_map(StepView::step_id)
        .filter(|id| read.iter().any(|i| names(i, id)))
        .filter_map(|id| id.split_once('/').map(|(g, _)| g.to_string()))
        .filter(|g| !searched.contains(g.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let gone = keyword_steps(&view);
    if gone.is_empty() {
        return Ok(text.to_string());
    }
    // The index group under any name it has had: the rename may come later
    // in the chain than this.
    let index_group = std::iter::once(INDEX_GROUP)
        .chain(OLD_INDEX_GROUPS)
        .find(|g| view.groups.iter().any(|d| d.id == *g))
        .unwrap_or(INDEX_GROUP);
    let grid = view.steps.iter().find(|s| s.is(index_group, GRID_INDEX));
    let grid_id = format!("{index_group}/{GRID_INDEX}");
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for range in entry_ranges(text) {
        if is_one_of(&text[range.clone()], &gone) {
            edits.push((range, String::new()));
        }
    }
    let names_a_keyword_step =
        |input: &str| gone.contains(input) || input == format!("*/{KEYWORD_INDEX}");
    for step in &view.steps {
        let Some(inputs) = &step.inputs else { continue };
        if !inputs.get_ref().iter().any(|i| names_a_keyword_step(i)) {
            continue;
        }
        let mut after: Vec<String> = Vec::new();
        for input in inputs.get_ref() {
            let input = if names_a_keyword_step(input) {
                grid_id.clone()
            } else {
                input.clone()
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
    let listed = |left_out: &[String]| {
        let names: Vec<String> = left_out.iter().map(|g| format!("{g:?}")).collect();
        format!(
            "\n# Kept out of search: these sources had no keyword_index step.\n\
             params.not_searched = [{}]",
            names.join(", ")
        )
    };
    match grid {
        Some(grid) => {
            let read = grid
                .inputs
                .as_ref()
                .map(|i| i.get_ref().clone())
                .unwrap_or_default();
            let left_out = not_searched(&view, &read, &gone);
            if !left_out.is_empty() {
                if grid.params.is_some() {
                    bail!(
                        "`{grid_id}` already has params, so the sources with no \
                         `keyword_index` cannot be listed in its `not_searched` without \
                         asking: {}",
                        left_out.join(", ")
                    );
                }
                let Some(inputs) = &grid.inputs else {
                    bail!("`{grid_id}` reads nothing, so no source was ever searched");
                };
                let at = inputs.span().end;
                edits.push((at..at, listed(&left_out)));
            }
        }
        None => {
            let read: Vec<String> = GRID_READS.iter().map(|r| r.to_string()).collect();
            let left_out = not_searched(&view, &read, &gone);
            let group = if view.groups.iter().any(|g| g.id == index_group) {
                String::new()
            } else {
                format!("[[groups]]\nid = {index_group:?}\n\n")
            };
            let inputs: Vec<String> = read.iter().map(|r| format!("{r:?}")).collect();
            let params = if left_out.is_empty() {
                String::new()
            } else {
                listed(&left_out)
            };
            let at = text.len();
            edits.push((
                at..at,
                format!(
                    "\n# The grid, and the keyword search the keyword_index steps did.\n\
                     {group}[[steps]]\ngroup = {index_group:?}\nfunction = {GRID_INDEX:?}\n\
                     command = \"datalib-step\"\ninputs = [{}]{params}\n",
                    inputs.join(", ")
                ),
            ));
        }
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
function = "ingest"
command = "datalib-step"

[[steps]]
group = "picard"
function = "render_markdown"
command = "datalib-step"
inputs = ["picard/ingest"]

# the captain's mail, in search
[[steps]]
group = "picard"
function = "keyword_index"
command = "datalib-step"
inputs = ["picard/render_markdown"]

[[steps]]
group = "picard"
function = "embed"
command = "datalib-step"
inputs = ["picard/keyword_index"]

[[groups]]
id = "q"
type = "email"

[[steps]]
group = "q"
function = "ingest"
command = "datalib-step"

[[steps]]
group = "q"
function = "render_markdown"
command = "datalib-step"
inputs = ["q/ingest"]

[[groups]]
id = "unified_grid_and_search_index"

[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"
command = "datalib-step"
inputs = ["*/render_markdown"]

[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
command = "datalib-step"
inputs = ["*/keyword_index", "*/embed"]
"#;

    fn check(text: &str) -> datalib_dag::config::ConfigCheck {
        let check = datalib_dag::config::check_text(text);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        check
    }

    fn inputs_of(check: &datalib_dag::config::ConfigCheck, id: &str) -> Vec<String> {
        check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .inputs
            .clone()
    }

    /// The keyword step and its comment go; the embed, and the map that
    /// read every keyword step, read the `grid_index` step; the source that
    /// had no keyword step is listed in `not_searched`, so it stays out of
    /// search; the result loads whole, and a second pass finds nothing to
    /// do.
    #[test]
    fn keyword_steps_fold_into_the_grid_step() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains("keyword_index\"\ncommand"), "{after}");
        assert!(!after.contains("the captain's mail, in search"), "{after}");
        let check = check(&after);
        let grid = "unified_grid_and_search_index/grid_index";
        assert_eq!(inputs_of(&check, "picard/embed"), [grid]);
        let mut mapped = inputs_of(&check, "unified_grid_and_search_index/embedding_map");
        mapped.sort();
        assert_eq!(mapped, ["picard/embed", grid]);
        let params = check
            .cfg
            .steps
            .iter()
            .find(|s| s.id == grid)
            .unwrap()
            .params
            .clone()
            .unwrap();
        assert_eq!(
            params.get("not_searched").unwrap().as_array().unwrap(),
            &vec![toml::Value::String("q".into())]
        );
        assert!(!is_retired(&after).unwrap());
    }

    /// Every source searched leaves nothing to list.
    #[test]
    fn a_config_that_searched_everything_lists_nothing() {
        let all = BEFORE.replace(
            "[[groups]]\nid = \"unified_grid_and_search_index\"",
            "[[steps]]\ngroup = \"q\"\nfunction = \"keyword_index\"\ncommand = \"datalib-step\"\n\
             inputs = [\"q/render_markdown\"]\n\n[[groups]]\nid = \"unified_grid_and_search_index\"",
        );
        let after = rewrite(&all).unwrap();
        assert!(!after.contains("not_searched"), "{after}");
        check(&after);
    }

    /// With no `grid_index` step, the rewrite adds one reading every render, so
    /// the keyword search the steps did goes on.
    #[test]
    fn a_config_with_no_grid_step_gets_one() {
        let no_grid = BEFORE.replace(
            "[[steps]]\ngroup = \"unified_grid_and_search_index\"\nfunction = \"grid_index\"\n\
             command = \"datalib-step\"\n\
             inputs = [\"*/render_markdown\"]\n",
            "",
        );
        assert!(!no_grid.contains("grid_index"), "{no_grid}");
        let after = rewrite(&no_grid).unwrap();
        let check = check(&after);
        assert_eq!(
            inputs_of(&check, "unified_grid_and_search_index/grid_index"),
            ["picard/render_markdown", "q/render_markdown"]
        );
        assert!(after.contains("params.not_searched = [\"q\"]"), "{after}");
    }
}
