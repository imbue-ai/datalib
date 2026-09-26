//! The shape from before a source's qmd work was its own: one
//! `unified_index/qmd_index` step keyword-indexed and embedded every
//! source it named. Now that step only registers the collections, and each
//! source it names needs a `keyword_index` and an `embed` step of its own
//! to fill one.
//!
//! Unlike `convert.rs`, a text edit: the config is otherwise current, so
//! the missing steps are appended and the embedding map's `inputs` are
//! replaced where they stand, and every comment, lock and key the file
//! holds survives.

use std::collections::BTreeSet;

use anyhow::{Context as _, Result};
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
    command: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
}

impl StepView {
    fn id(&self) -> Option<String> {
        match (&self.group, &self.function) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }

    fn is_builtin(&self, function: &str) -> bool {
        self.command.is_none() && self.function.as_deref() == Some(function)
    }

    fn inputs(&self) -> &[String] {
        self.inputs.as_ref().map_or(&[], |s| s.get_ref())
    }
}

/// What the rewrite would add: each source a `qmd_index` names with no
/// `keyword_index`, paired with that fan-in's id.
fn missing(view: &View) -> Vec<(String, String)> {
    let declared: BTreeSet<String> = view.steps.iter().filter_map(StepView::id).collect();
    let mut out = Vec::new();
    for fan_in in view.steps.iter().filter(|s| s.is_builtin("qmd_index")) {
        let Some(fan_in_id) = fan_in.id() else {
            continue;
        };
        for group in fan_in
            .inputs()
            .iter()
            .filter_map(|i| i.strip_suffix("/render_markdown"))
        {
            if !declared.contains(&format!("{group}/keyword_index"))
                && !out.iter().any(|(g, _)| g == group)
            {
                out.push((group.to_string(), fan_in_id.clone()));
            }
        }
    }
    out
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(!missing(&view).is_empty())
}

fn quote_list(ids: &[String]) -> String {
    let quoted: Vec<String> = ids
        .iter()
        .map(|i| toml::Value::String(i.clone()).to_string())
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// Add each missing source's two steps, and point every embedding map
/// that read a `qmd_index` at the embed steps instead, which is where the
/// vectors it lays out now come from.
pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let add = missing(&view);
    if add.is_empty() {
        return Ok(text.to_string());
    }
    let declared: BTreeSet<String> = view.steps.iter().filter_map(StepView::id).collect();
    let fan_ins: BTreeSet<String> = view
        .steps
        .iter()
        .filter(|s| s.is_builtin("qmd_index"))
        .filter_map(StepView::id)
        .collect();

    let mut embeds: Vec<String> = view
        .steps
        .iter()
        .filter(|s| s.is_builtin("embed"))
        .filter_map(StepView::id)
        .collect();
    let mut blocks = Vec::new();
    for (group, fan_in) in &add {
        let g = toml::Value::String(group.clone()).to_string();
        let keyword = format!("{group}/keyword_index");
        blocks.push(format!(
            "[[steps]]\ngroup = {g}\nfunction = \"keyword_index\"\ninputs = {}\n",
            quote_list(&[format!("{group}/render_markdown"), fan_in.clone()])
        ));
        let embed = format!("{group}/embed");
        if !declared.contains(&embed) {
            blocks.push(format!(
                "[[steps]]\ngroup = {g}\nfunction = \"embed\"\ninputs = {}\n",
                quote_list(&[keyword])
            ));
            embeds.push(embed);
        }
    }

    // Spans are into the original text, so the replacements go last
    // first and leave each earlier span where it was.
    let mut out = text.to_string();
    let mut maps: Vec<std::ops::Range<usize>> = view
        .steps
        .iter()
        .filter(|s| s.is_builtin("embedding_map"))
        .filter(|s| s.inputs().iter().any(|i| fan_ins.contains(i)))
        .filter_map(|s| s.inputs.as_ref().map(toml::Spanned::span))
        .collect();
    maps.sort_by_key(|r| std::cmp::Reverse(r.start));
    for span in maps {
        out.replace_range(span, &quote_list(&embeds));
    }

    let mut out = format!("{}\n", out.trim_end());
    out.push_str(
        "\n# Added by datalib-migrate-config: each source's own qmd steps, which\n\
         # keyword-index and embed what `qmd_index` used to do for all of them.\n",
    );
    for b in blocks {
        out.push('\n');
        out.push_str(&b);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"# my sources
[[groups]]
id = "mail"
type = "email"

[[steps]]
group = "mail"
function = "ingest"
locks = ["mine"]
[steps.params.mbox]
path = "/m"

[[steps]]
group = "mail"
function = "render_markdown"
inputs = ["mail/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["mail/render_markdown"]

[[steps]]
group = "unified_index"
function = "embedding_map"
inputs = ["unified_index/qmd_index"] # the map

[[locks]]
name = "mine"
"#;

    #[test]
    fn the_fan_in_shape_is_retired_and_the_current_one_is_not() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!is_retired(&after).unwrap(), "{after}");
        assert_eq!(
            rewrite(&after).unwrap(),
            after,
            "a second run changes nothing"
        );
    }

    /// The point of a text edit: nothing the file held is lost, and what
    /// it adds is the steps and the map's new inputs.
    #[test]
    fn comments_locks_and_other_keys_survive() {
        let after = rewrite(BEFORE).unwrap();
        for kept in [
            "# my sources",
            "locks = [\"mine\"]",
            "[[locks]]",
            "# the map",
        ] {
            assert!(after.contains(kept), "lost {kept:?}:\n{after}");
        }
        assert!(
            after.contains("inputs = [\"mail/render_markdown\", \"unified_index/qmd_index\"]"),
            "{after}"
        );
        assert!(after.contains("function = \"embed\"\ninputs = [\"mail/keyword_index\"]"));
        assert!(
            after.contains("function = \"embedding_map\"\ninputs = [\"mail/embed\"] # the map"),
            "{after}"
        );
    }

    #[test]
    fn the_result_loads_clean() {
        let after = rewrite(BEFORE).unwrap();
        let check = datalib_dag::config::check_text(&after);
        assert!(check.is_clean(), "{:?}\n{after}", check.diagnostics);
    }

    /// A source whose embed a person already added keeps it, and gets the
    /// keyword step it was missing.
    #[test]
    fn an_existing_embed_step_is_not_duplicated() {
        let with_embed = format!(
            "{BEFORE}\n[[steps]]\ngroup = \"mail\"\nfunction = \"embed\"\ninputs = [\"mail/keyword_index\"]\n"
        );
        let after = rewrite(&with_embed).unwrap();
        assert_eq!(after.matches("function = \"embed\"").count(), 1, "{after}");
        assert_eq!(after.matches("function = \"keyword_index\"").count(), 1);
    }
}
