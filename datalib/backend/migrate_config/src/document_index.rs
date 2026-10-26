//! The shape from before the unified index was named for what it indexes:
//! the group, its steps and its applet were `unified_index`, and are now
//! `unified_document_index`. The rewrite renames the group, every step
//! filed under it or writing under it, every `inputs` entry naming one of
//! those steps, and the applet's `group`, `id` and `datalib-applet`
//! subcommand.
//!
//! A text edit, like `qmd_steps.rs`: values are replaced where they
//! stand, so every comment and key the file holds survives. Only the
//! untyped group is renamed; a source named `unified_index` is not the
//! index and is left for the loader to report.

use std::ops::Range;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

use datalib_dag::config_array::edit_string_array;

pub const OLD: &str = "unified_index";
pub const NEW: &str = "unified_document_index";
/// What the scaffold named the group; a name a person typed is theirs.
const OLD_NAME: &str = "Unified Index";
const NEW_NAME: &str = "Document Index";

#[derive(Deserialize)]
struct View {
    #[serde(default)]
    groups: Vec<GroupView>,
    #[serde(default)]
    steps: Vec<StepView>,
    #[serde(default)]
    applets: Vec<AppletView>,
}

#[derive(Deserialize)]
struct GroupView {
    id: Spanned<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    name: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    id: Option<Spanned<String>>,
    #[serde(default)]
    group: Option<Spanned<String>>,
    #[serde(default)]
    inputs: Option<Spanned<Vec<String>>>,
}

#[derive(Deserialize)]
struct AppletView {
    #[serde(default)]
    id: Option<Spanned<String>>,
    #[serde(default)]
    group: Option<Spanned<String>>,
    #[serde(default)]
    command: Option<Spanned<String>>,
}

/// `unified_index/x` → `unified_document_index/x`, and `unified_index`
/// itself; `None` for anything else.
pub fn renamed(id: &str) -> Option<String> {
    if id == OLD {
        return Some(NEW.to_string());
    }
    id.strip_prefix(OLD)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| format!("{NEW}/{rest}"))
}

/// The command with its `datalib-applet unified_index` subcommand
/// renamed; `None` when it runs something else.
fn renamed_command(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    let program = words.next()?.trim_matches(|c| c == '\'' || c == '"');
    let base = program.rsplit('/').next().unwrap_or(program);
    if base != "datalib-applet" && base != "datalib_applet" {
        return None;
    }
    if words.next()? != OLD {
        return None;
    }
    let at = command.find(program)? + program.len();
    let word = at + command[at..].find(OLD)?;
    let mut out = command.to_string();
    out.replace_range(word..word + OLD.len(), NEW);
    Some(out)
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn edits(text: &str) -> Result<Vec<(Range<usize>, String)>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let index_groups: Vec<&GroupView> = view
        .groups
        .iter()
        .filter(|g| g.r#type.is_none() && g.id.get_ref() == OLD)
        .collect();
    // A source that took the name is not the index, and neither is
    // anything filed under it.
    let a_source_has_it = view
        .groups
        .iter()
        .any(|g| g.r#type.is_some() && g.id.get_ref() == OLD);
    if a_source_has_it {
        return Ok(Vec::new());
    }

    let ids = index_groups
        .iter()
        .map(|g| &g.id)
        .chain(
            view.steps
                .iter()
                .filter_map(|s| s.group.as_ref().or(s.id.as_ref())),
        )
        .chain(
            view.applets
                .iter()
                .flat_map(|a| a.group.iter().chain(&a.id)),
        );
    let mut out: Vec<(Range<usize>, String)> = ids
        .filter_map(|s| renamed(s.get_ref()).map(|new| (s.span(), quote(&new))))
        .collect();
    for g in &index_groups {
        if let Some(name) = g.name.as_ref().filter(|n| n.get_ref() == OLD_NAME) {
            out.push((name.span(), quote(NEW_NAME)));
        }
    }
    for c in view.applets.iter().filter_map(|a| a.command.as_ref()) {
        if let Some(new) = renamed_command(c.get_ref()) {
            out.push((c.span(), quote(&new)));
        }
    }
    for i in view.steps.iter().filter_map(|s| s.inputs.as_ref()) {
        if i.get_ref().iter().any(|x| renamed(x).is_some()) {
            let after: Vec<String> = i
                .get_ref()
                .iter()
                .map(|x| renamed(x).unwrap_or_else(|| x.clone()))
                .collect();
            out.push((
                i.span(),
                rename_in_array(&text[i.span()], &after).map_err(anyhow::Error::msg)?,
            ));
        }
    }
    Ok(out)
}

/// `edit_string_array` keeps the strings it already holds and appends the
/// rest, so a rename goes one id at a time: each new id first lands at the
/// end, then moves to where the old one stood by a plain token swap.
fn rename_in_array(written: &str, after: &[String]) -> Result<String, String> {
    let mut out = written.to_string();
    for new in after {
        let Some(old) = new
            .strip_prefix(NEW)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("{OLD}{rest}"))
        else {
            continue;
        };
        let token = quote(&old);
        if let Some(at) = out.find(&token) {
            out.replace_range(at..at + token.len(), &quote(new));
        }
    }
    // The read-back check: the swap must spell exactly `after`.
    edit_string_array(&out, after)
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!edits(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut edits = edits(text)?;
    // Spans index the original text, so replacing the last first leaves
    // every earlier span where it was.
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

    /// The scaffold's index as an earlier build wrote it, with a source
    /// feeding it and a person's comments in among the entries.
    const OLD_SHAPE: &str = r#"# my sources
[[groups]]
id = "picard"
type = "email"
name = "Captain's log"

[[steps]]
group = "picard"
function = "ingest"
[steps.params.mbox]
path = "/enterprise/picard.mbox"

[[steps]]
group = "picard"
function = "render_markdown"
inputs = ["picard/ingest"]

[[steps]]
group = "picard"
function = "keyword_index"
inputs = ["picard/render_markdown"]

# ── the unified index ──
[[groups]]
id = "unified_index"
name = "Unified Index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["picard/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_aggregator"
inputs = ["picard/keyword_index"]

[[steps]]
group = "unified_index"
function = "embedding_map"
inputs = [
  # the map reads the aggregator
  "unified_index/qmd_aggregator",
]

[[steps]]
id = "exports/csv"
command = "my-exporter"
inputs = ["picard/render_markdown", "unified_index/grid_index"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index" # serves the grid
"#;

    fn step_ids(text: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(text);
        assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
        check.cfg.steps.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn renames_the_group_its_steps_their_readers_and_the_applet() {
        assert!(is_retired(OLD_SHAPE).unwrap());
        let after = rewrite(OLD_SHAPE).unwrap();
        assert!(!is_retired(&after).unwrap(), "{after}");
        assert!(!after.contains("\"unified_index"), "{after}");
        assert_eq!(
            step_ids(&after),
            [
                "picard/ingest",
                "picard/render_markdown",
                "picard/keyword_index",
                "unified_document_index/grid_index",
                "unified_document_index/qmd_aggregator",
                "unified_document_index/embedding_map",
                "exports/csv",
            ]
        );
        let check = datalib_dag::config::check_text(&after);
        let applet = &check.cfg.applets[0];
        assert_eq!(applet.id, "unified_document_index");
        assert_eq!(applet.group.as_deref(), Some("unified_document_index"));
        assert_eq!(applet.command, "datalib-applet unified_document_index");
        assert!(
            after.contains(
                "inputs = [\"picard/render_markdown\", \"unified_document_index/grid_index\"]"
            ),
            "{after}"
        );
        assert!(after.contains("name = \"Document Index\""), "{after}");
    }

    #[test]
    fn comments_and_layout_survive() {
        let after = rewrite(OLD_SHAPE).unwrap();
        for kept in [
            "# my sources",
            "# ── the unified index ──",
            "name = \"Captain's log\"",
            "inputs = [\n  # the map reads the aggregator\n  \"unified_document_index/qmd_aggregator\",\n]",
            "# serves the grid",
        ] {
            assert!(after.contains(kept), "lost {kept:?}:\n{after}");
        }
    }

    #[test]
    fn a_current_config_and_a_name_a_person_typed_are_left_alone() {
        let after = rewrite(OLD_SHAPE).unwrap();
        assert_eq!(rewrite(&after).unwrap(), after);
        let named = OLD_SHAPE.replace("name = \"Unified Index\"", "name = \"Everything\"");
        assert!(rewrite(&named).unwrap().contains("name = \"Everything\""));
    }

    /// Only the untyped group is the index: a source that took the name
    /// keeps it, and so does everything filed under it.
    #[test]
    fn a_source_named_unified_index_is_not_the_index() {
        let text = "[[groups]]\nid = \"unified_index\"\ntype = \"email\"\n\n\
                    [[steps]]\ngroup = \"unified_index\"\nfunction = \"ingest\"\n";
        assert!(!is_retired(text).unwrap());
        assert_eq!(rewrite(text).unwrap(), text);
    }

    #[test]
    fn the_applet_command_keeps_its_path_and_quoting() {
        assert_eq!(
            renamed_command("'/x/bin/datalib_applet' unified_index --flag").as_deref(),
            Some("'/x/bin/datalib_applet' unified_document_index --flag")
        );
        assert_eq!(renamed_command("python unified_index.py"), None);
        assert_eq!(renamed_command("datalib-applet slack"), None);
    }
}
