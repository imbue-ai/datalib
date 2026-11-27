//! A config whose index step's function is `grid_index`. The step is now
//! `unified_grid_and_search_index`, named for the group it is filed under
//! and the grid and the search it fills, so its id is
//! `unified_grid_and_search_index/unified_grid_and_search_index`. The
//! rewrite renames the `function` of every built-in `grid_index` step and
//! every `inputs` entry that names one (each source's `embed` reads it),
//! and changes nothing else.
//!
//! The step's tree moves with it: `datalib-http` moves
//! `<group>/grid_index/` at boot (`config_upgrade::move_index_step_dir`).

use std::collections::BTreeMap;
use std::ops::Range;

use crate::retired::names_datalib_step;
use anyhow::{Context as _, Result};
use datalib_config::array::edit_string_array;
use serde::Deserialize;
use toml::Spanned;

pub const OLD: &str = "grid_index";
pub const NEW: &str = "unified_grid_and_search_index";

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
    function: Option<Spanned<String>>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    inputs: Option<Spanned<Vec<String>>>,
}

impl StepView {
    fn is_retired(&self) -> bool {
        names_datalib_step(self.command.as_deref())
            && self.group.is_some()
            && self.function.as_ref().is_some_and(|f| f.get_ref() == OLD)
    }
}

/// Each renamed step's old id and its new one.
fn renamed(view: &View) -> BTreeMap<String, String> {
    view.steps
        .iter()
        .filter(|s| s.is_retired())
        .filter_map(|s| s.group.as_deref())
        .map(|g| (format!("{g}/{OLD}"), format!("{g}/{NEW}")))
        .collect()
}

/// `<group>/grid_index` → `<group>/unified_grid_and_search_index`, for an
/// id the rewrite of `text` renames; `None` for anything else.
pub fn renamed_id(text: &str, id: &str) -> Option<String> {
    let view: View = toml::from_str(text).ok()?;
    renamed(&view).remove(id)
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(view.steps.iter().any(StepView::is_retired))
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let ids = renamed(&view);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for step in &view.steps {
        if let Some(function) = step.function.as_ref().filter(|_| step.is_retired()) {
            edits.push((function.span(), toml::Value::from(NEW).to_string()));
        }
        let Some(inputs) = &step.inputs else {
            continue;
        };
        if !inputs.get_ref().iter().any(|i| ids.contains_key(i)) {
            continue;
        }
        let after: Vec<String> = inputs
            .get_ref()
            .iter()
            .map(|i| ids.get(i).unwrap_or(i).clone())
            .collect();
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

# the grid and the search
[[steps]]
group = "unified_grid_and_search_index"
function = "grid_index"  # every source
command = "datalib-step"
inputs = ["*/render_markdown"]
params.not_searched = ["q"]

[[steps]]
group = "unified_grid_and_search_index"
function = "embedding_map"
command = "datalib-step"
inputs = ["*/embed"]
"#;

    /// The step and the input that names it take the new name; its
    /// comments and params stay where they were; the result loads whole,
    /// and a second pass finds nothing to do.
    #[test]
    fn the_index_step_takes_its_groups_name() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains("grid_index"), "{after}");
        assert!(
            after.contains(
                "# the grid and the search\n[[steps]]\n\
                 group = \"unified_grid_and_search_index\"\n\
                 function = \"unified_grid_and_search_index\"  # every source\n"
            ),
            "{after}"
        );
        let check = crate::check_as_current(&after);
        assert_eq!(check.dropped(), 0, "{}", check.render("c.toml".as_ref()));
        let id = "unified_grid_and_search_index/unified_grid_and_search_index";
        let step = |id: &str| check.cfg.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(step("picard/embed").inputs, [id]);
        assert_eq!(
            step(id).params.as_ref().unwrap()["not_searched"],
            toml::Value::Array(vec!["q".into()])
        );
        assert!(!is_retired(&after).unwrap());
        assert_eq!(rewrite(&after).unwrap(), after);
    }

    /// A step that runs its own program is not Datalib's index, whatever
    /// its function is called, and neither is what reads it.
    #[test]
    fn a_custom_grid_index_is_left_alone() {
        let custom = BEFORE.replace(
            "function = \"grid_index\"  # every source\ncommand = \"datalib-step\"",
            "function = \"grid_index\"  # every source\ncommand = \"my-indexer\"",
        );
        assert!(!is_retired(&custom).unwrap());
        assert_eq!(rewrite(&custom).unwrap(), custom);
    }

    #[test]
    fn renamed_id_names_only_the_steps_it_renames() {
        assert_eq!(
            renamed_id(BEFORE, "unified_grid_and_search_index/grid_index").as_deref(),
            Some("unified_grid_and_search_index/unified_grid_and_search_index")
        );
        assert_eq!(renamed_id(BEFORE, "picard/embed"), None);
    }
}
