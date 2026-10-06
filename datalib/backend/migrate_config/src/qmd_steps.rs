//! The two shapes from before `qmd_aggregator`, when a
//! `unified_index/qmd_index` step read every source's render: it either
//! keyword-indexed and embedded every source itself, or registered the
//! collections that each source's `keyword_index`, reading it, then filled.
//!
//! Now each source's `keyword_index` reads only its render, its `embed`
//! reads that, and `qmd_aggregator` reads both, downstream of them all.
//! The rewrite renames the step, points it at the per-source steps, adds
//! any a source lacks (both of them, in the first shape, which embedded
//! everything), and points the embedding map at the aggregator.
//!
//! Unlike `convert.rs`, a text edit: the config is otherwise current, so
//! values are replaced where they stand, down to how an `inputs` array was
//! laid out, and every comment, lock and key the file holds survives. The
//! missing steps are added and the file put back in the order data flows
//! (`datalib_dag::config_order`), so each lands below the render it reads
//! and the aggregator below them all.

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::sort_config;

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
    function: Option<toml::Spanned<String>>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
}

impl StepView {
    fn function(&self) -> Option<&str> {
        self.function.as_ref().map(|f| f.get_ref().as_str())
    }

    fn id(&self) -> Option<String> {
        match (&self.group, self.function()) {
            (Some(g), Some(f)) => Some(format!("{g}/{f}")),
            _ => self.id.clone(),
        }
    }

    fn is_builtin(&self, function: &str) -> bool {
        self.command.is_none() && self.function() == Some(function)
    }

    fn inputs(&self) -> &[String] {
        self.inputs.as_ref().map_or(&[], |s| s.get_ref())
    }
}

pub fn is_retired(text: &str) -> Result<bool> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(view.steps.iter().any(|s| s.is_builtin("qmd_index")))
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn quote_list(ids: &[String]) -> String {
    let quoted: Vec<String> = ids.iter().map(|i| quote(i)).collect();
    format!("[{}]", quoted.join(", "))
}

fn block(group: &str, function: &str, inputs: &[String]) -> String {
    format!(
        "[[steps]]\ngroup = {}\nfunction = {}\ninputs = {}\n",
        quote(group),
        quote(function),
        quote_list(inputs)
    )
}

fn push_unique(list: &mut Vec<String>, item: &str) {
    if !list.iter().any(|x| x == item) {
        list.push(item.to_string());
    }
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let olds: Vec<&StepView> = view
        .steps
        .iter()
        .filter(|s| s.is_builtin("qmd_index"))
        .collect();
    if olds.is_empty() {
        return Ok(text.to_string());
    }
    let old_ids: BTreeSet<String> = olds.iter().filter_map(|s| s.id()).collect();
    let declared: BTreeSet<String> = view.steps.iter().filter_map(StepView::id).collect();
    let keywords: Vec<&StepView> = view
        .steps
        .iter()
        .filter(|s| s.is_builtin("keyword_index"))
        .collect();
    // No source step of its own anywhere: the fan-in embedded everything.
    let embedded_everything = keywords.is_empty();

    let mut groups: Vec<String> = Vec::new();
    for old in &olds {
        for g in old
            .inputs()
            .iter()
            .filter_map(|i| i.strip_suffix("/render_markdown"))
        {
            push_unique(&mut groups, g);
        }
    }
    for g in keywords.iter().filter_map(|k| k.group.as_deref()) {
        push_unique(&mut groups, g);
    }

    let mut blocks = Vec::new();
    let mut aggregated = Vec::new();
    for g in &groups {
        let keyword = format!("{g}/keyword_index");
        if !declared.contains(&keyword) {
            blocks.push(block(g, "keyword_index", &[format!("{g}/render_markdown")]));
        }
        aggregated.push(keyword.clone());
        let embed = format!("{g}/embed");
        if declared.contains(&embed) {
            aggregated.push(embed);
        } else if embedded_everything {
            blocks.push(block(g, "embed", &[keyword]));
            aggregated.push(embed);
        }
    }

    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let mut aggregator_id = None;
    for old in &olds {
        if let Some(f) = &old.function {
            edits.push((f.span(), quote("qmd_aggregator")));
        }
        if let Some(i) = &old.inputs {
            edits.push((
                i.span(),
                edit_string_array(&text[i.span()], &aggregated).map_err(anyhow::Error::msg)?,
            ));
        }
        aggregator_id = old.group.as_ref().map(|g| format!("{g}/qmd_aggregator"));
    }
    for i in keywords.iter().filter_map(|k| k.inputs.as_ref()) {
        if i.get_ref().iter().any(|x| old_ids.contains(x)) {
            let kept: Vec<String> = i
                .get_ref()
                .iter()
                .filter(|x| !old_ids.contains(*x))
                .cloned()
                .collect();
            edits.push((
                i.span(),
                edit_string_array(&text[i.span()], &kept).map_err(anyhow::Error::msg)?,
            ));
        }
    }
    if let Some(aggregator) = &aggregator_id {
        for map in view.steps.iter().filter(|s| s.is_builtin("embedding_map")) {
            if let Some(i) = &map.inputs {
                let only = std::slice::from_ref(aggregator);
                edits.push((
                    i.span(),
                    edit_string_array(&text[i.span()], only).map_err(anyhow::Error::msg)?,
                ));
            }
        }
    }

    // Spans index the original text, so replacing the last first leaves
    // every earlier span where it was.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
    }
    if !blocks.is_empty() {
        out = format!("{}\n", out.trim_end());
        for b in blocks {
            out.push('\n');
            out.push_str(&b);
        }
    }
    let sorted = sort_config(&out)
        .map_err(anyhow::Error::msg)
        .context("put the rewritten config in data-flow order")?;
    Ok(sorted.unwrap_or(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r#"# my sources
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
id = "notes"
type = "email"

[[steps]]
group = "notes"
function = "ingest"
[steps.params.mbox]
path = "/n"

[[steps]]
group = "notes"
function = "render_markdown"
inputs = ["notes/ingest"]

[[groups]]
id = "unified_index"

[[locks]]
name = "mine"
"#;

    /// The first shape: the fan-in did everything, and no source had a
    /// qmd step of its own.
    fn everything_in_the_fan_in() -> String {
        format!(
            "{HEAD}
[[steps]]
group = \"unified_index\"
function = \"qmd_index\"
inputs = [\"mail/render_markdown\", \"notes/render_markdown\"]

[[steps]]
group = \"unified_index\"
function = \"embedding_map\"
inputs = [\"unified_index/qmd_index\"] # the map
"
        )
    }

    /// The second shape: the fan-in registered, the per-source steps read
    /// it, and only `mail` embedded.
    fn fan_in_upstream() -> String {
        format!(
            "{HEAD}
[[steps]]
group = \"unified_index\"
function = \"qmd_index\"
inputs = [\"mail/render_markdown\", \"notes/render_markdown\"]

[[steps]]
group = \"mail\"
function = \"keyword_index\"
inputs = [\"mail/render_markdown\", \"unified_index/qmd_index\"]

[[steps]]
group = \"mail\"
function = \"embed\"
inputs = [\"mail/keyword_index\"]

[[steps]]
group = \"notes\"
function = \"keyword_index\"
inputs = [\"notes/render_markdown\", \"unified_index/qmd_index\"]

[[steps]]
group = \"unified_index\"
function = \"embedding_map\"
inputs = [\"mail/embed\"]
"
        )
    }

    fn inputs_of(text: &str, id: &str) -> Vec<String> {
        let view: View = toml::from_str(text).unwrap();
        view.steps
            .iter()
            .find(|s| s.id().as_deref() == Some(id))
            .unwrap_or_else(|| panic!("no step {id}:\n{text}"))
            .inputs()
            .to_vec()
    }

    fn loads_clean(text: &str) {
        let check = datalib_dag::config::check_text(text);
        assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
    }

    #[test]
    fn the_first_shape_gains_both_steps_per_source_and_an_aggregator() {
        let before = everything_in_the_fan_in();
        assert!(is_retired(&before).unwrap());
        let after = rewrite(&before).unwrap();
        assert!(!is_retired(&after).unwrap(), "{after}");
        assert_eq!(
            inputs_of(&after, "mail/keyword_index"),
            ["mail/render_markdown"]
        );
        assert_eq!(inputs_of(&after, "notes/embed"), ["notes/keyword_index"]);
        assert_eq!(
            inputs_of(&after, "unified_index/qmd_aggregator"),
            [
                "mail/keyword_index",
                "mail/embed",
                "notes/keyword_index",
                "notes/embed"
            ]
        );
        assert_eq!(
            inputs_of(&after, "unified_index/embedding_map"),
            ["unified_index/qmd_aggregator"]
        );
        loads_clean(&after);
    }

    /// Only the source that embedded keeps embedding: the second shape
    /// already says which, and the rewrite adds no `embed` to it.
    #[test]
    fn the_second_shape_moves_the_edges_and_keeps_its_embeds() {
        let after = rewrite(&fan_in_upstream()).unwrap();
        assert_eq!(
            inputs_of(&after, "notes/keyword_index"),
            ["notes/render_markdown"]
        );
        assert_eq!(
            inputs_of(&after, "unified_index/qmd_aggregator"),
            ["mail/keyword_index", "mail/embed", "notes/keyword_index"]
        );
        assert!(
            !after.contains("group = \"notes\"\nfunction = \"embed\""),
            "{after}"
        );
        assert_eq!(
            inputs_of(&after, "unified_index/embedding_map"),
            ["unified_index/qmd_aggregator"]
        );
        loads_clean(&after);
    }

    /// The point of a text edit: nothing the file held is lost.
    #[test]
    fn comments_locks_and_other_keys_survive() {
        let after = rewrite(&everything_in_the_fan_in()).unwrap();
        for kept in [
            "# my sources",
            "locks = [\"mine\"]",
            "[[locks]]",
            "# the map",
        ] {
            assert!(after.contains(kept), "lost {kept:?}:\n{after}");
        }
    }

    /// #897: the rewrite wrote every `inputs` it replaced on one line,
    /// and a comment inside one went with the old ids.
    #[test]
    fn a_one_id_per_line_array_stays_one_id_per_line() {
        let before = everything_in_the_fan_in().replace(
            "inputs = [\"mail/render_markdown\", \"notes/render_markdown\"]",
            "inputs = [\n  # every source\n  \"mail/render_markdown\",\n  \"notes/render_markdown\",\n]",
        );
        let after = rewrite(&before).unwrap();
        let aggregator = "function = \"qmd_aggregator\"\ninputs = [
  # every source
  \"mail/keyword_index\",
  \"mail/embed\",
  \"notes/keyword_index\",
  \"notes/embed\",
]\n";
        assert!(after.contains(aggregator), "{after}");
        assert!(
            after.contains("inputs = [\"unified_index/qmd_aggregator\"] # the map"),
            "{after}"
        );
        loads_clean(&after);
    }

    /// The added steps land beside their source, below the render they
    /// read, rather than at the end of the file below the aggregator
    /// that reads them.
    #[test]
    fn the_rewrite_leaves_the_file_in_data_flow_order() {
        for before in [everything_in_the_fan_in(), fan_in_upstream()] {
            let after = rewrite(&before).unwrap();
            assert_eq!(sort_config(&after), Ok(None), "{after}");
            let at = |needle: &str| after.find(needle).unwrap_or_else(|| panic!("{needle}"));
            assert!(
                at("group = \"mail\"\nfunction = \"keyword_index\"")
                    < at("[[groups]]\nid = \"notes\""),
                "{after}"
            );
        }
    }

    #[test]
    fn a_second_run_changes_nothing() {
        for before in [everything_in_the_fan_in(), fan_in_upstream()] {
            let after = rewrite(&before).unwrap();
            assert_eq!(rewrite(&after).unwrap(), after);
        }
    }
}
