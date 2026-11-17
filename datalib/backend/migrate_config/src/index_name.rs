//! The index group's earlier names: `unified_index`, then
//! `unified_document_index`, and now `unified_grid_and_search_index`. The
//! rewrite renames the group, every step filed under it or writing under
//! it, every `inputs` entry naming one of those steps, the applet's
//! `group`, `id` and `datalib-applet` subcommand, and the name and banner
//! the scaffold gave the group.
//!
//! A text edit, like `qmd_steps.rs`: values are replaced where they
//! stand, so every comment and key the file holds survives. Only the
//! untyped group is renamed; a source that took one of the old names is
//! not the index and is left for the loader to report.

use std::ops::Range;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

use datalib_dag::config_array::edit_string_array;

/// The group's earlier ids, newest first.
pub const OLD: [&str; 2] = ["unified_document_index", "unified_index"];
pub const NEW: &str = "unified_grid_and_search_index";
/// What the scaffold named the group; a name a person typed is theirs.
const OLD_NAMES: [&str; 2] = ["Document Index", "Unified Index"];
const NEW_NAME: &str = "Grid and Search Index";
/// The scaffold's banner above the group, as earlier builds wrote it.
const OLD_BANNERS: [&str; 2] = [
    "# ── the unified document index ──",
    "# ── the unified index ──",
];
const NEW_BANNER: &str = "# ── the unified grid and search index ──";

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

/// `<old>/x` → `unified_grid_and_search_index/x`, and `<old>` itself, for
/// each of the group's earlier ids; `None` for anything else.
pub fn renamed(id: &str) -> Option<String> {
    renamed_from(id, &OLD)
}

fn renamed_from(id: &str, olds: &[&str]) -> Option<String> {
    olds.iter().find_map(|old| {
        if id == *old {
            return Some(NEW.to_string());
        }
        id.strip_prefix(old)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("{NEW}/{rest}"))
    })
}

/// The command with its `datalib-applet <old>` subcommand renamed; `None`
/// when it runs something else.
fn renamed_command(command: &str, olds: &[&str]) -> Option<String> {
    let mut words = command.split_whitespace();
    let program = words.next()?.trim_matches(|c| c == '\'' || c == '"');
    let base = program.rsplit('/').next().unwrap_or(program);
    if base != "datalib-applet" && base != "datalib_applet" {
        return None;
    }
    let sub = words.next()?;
    let old = olds.iter().copied().find(|o| *o == sub)?;
    let at = command.find(program)? + program.len();
    let word = at + command[at..].find(old)?;
    let mut out = command.to_string();
    out.replace_range(word..word + old.len(), NEW);
    Some(out)
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn edits(text: &str) -> Result<Vec<(Range<usize>, String)>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    // A source that took an old name is not the index, and neither is
    // anything filed under it.
    let olds: Vec<&str> = OLD
        .into_iter()
        .filter(|old| {
            !view
                .groups
                .iter()
                .any(|g| g.r#type.is_some() && g.id.get_ref() == *old)
        })
        .collect();
    let index_groups: Vec<&GroupView> = view
        .groups
        .iter()
        .filter(|g| g.r#type.is_none() && olds.contains(&g.id.get_ref().as_str()))
        .collect();

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
        .filter_map(|s| renamed_from(s.get_ref(), &olds).map(|new| (s.span(), quote(&new))))
        .collect();
    for g in &index_groups {
        if let Some(name) = g
            .name
            .as_ref()
            .filter(|n| OLD_NAMES.contains(&n.get_ref().as_str()))
        {
            out.push((name.span(), quote(NEW_NAME)));
        }
    }
    for c in view.applets.iter().filter_map(|a| a.command.as_ref()) {
        if let Some(new) = renamed_command(c.get_ref(), &olds) {
            out.push((c.span(), quote(&new)));
        }
    }
    for i in view.steps.iter().filter_map(|s| s.inputs.as_ref()) {
        if i.get_ref().iter().any(|x| renamed_from(x, &olds).is_some()) {
            out.push((
                i.span(),
                rename_in_array(&text[i.span()], i.get_ref(), &olds).map_err(anyhow::Error::msg)?,
            ));
        }
    }
    if !index_groups.is_empty() {
        out.extend(banner_edits(text));
    }
    Ok(out)
}

/// The scaffold's banner, renamed where an earlier build wrote it; the
/// rule of `─` after it is left as long as it was.
fn banner_edits(text: &str) -> Vec<(Range<usize>, String)> {
    OLD_BANNERS
        .iter()
        .flat_map(|old| {
            text.match_indices(old)
                .map(|(at, _)| (at..at + old.len(), NEW_BANNER.to_string()))
        })
        .collect()
}

/// `edit_string_array` keeps the strings it already holds and appends the
/// rest, so a rename swaps each old id's token for its new one where it
/// stands, then checks that the array now spells exactly the renamed list.
fn rename_in_array(written: &str, before: &[String], olds: &[&str]) -> Result<String, String> {
    let mut out = written.to_string();
    let after: Vec<String> = before
        .iter()
        .map(|x| renamed_from(x, olds).unwrap_or_else(|| x.clone()))
        .collect();
    for (old, new) in before.iter().zip(&after).filter(|(o, n)| o != n) {
        let token = quote(old);
        if let Some(at) = out.find(&token) {
            out.replace_range(at..at + token.len(), &quote(new));
        }
    }
    edit_string_array(&out, &after)
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

    /// The scaffold's index as the first build to name it wrote it, with a
    /// source feeding it and a person's comments in among the entries.
    const UNIFIED_INDEX_SHAPE: &str = r#"# my sources
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

    /// The same config as the build before this one wrote it.
    fn document_index_shape() -> String {
        UNIFIED_INDEX_SHAPE
            .replace("unified_index", "unified_document_index")
            .replace("Unified Index", "Document Index")
            .replace(
                "# ── the unified index ──",
                "# ── the unified document index ──────────",
            )
    }

    fn step_ids(text: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(&crate::folded(text));
        assert!(check.is_clean(), "{:?}\n{text}", check.diagnostics);
        check.cfg.steps.iter().map(|s| s.id.clone()).collect()
    }

    fn assert_renamed(before: &str) {
        assert!(is_retired(before).unwrap());
        let after = rewrite(before).unwrap();
        assert!(!is_retired(&after).unwrap(), "{after}");
        assert!(!after.contains("\"unified_index"), "{after}");
        assert!(!after.contains("unified_document_index"), "{after}");
        assert_eq!(
            step_ids(&after),
            [
                "picard/ingest",
                "picard/render_markdown",
                "unified_grid_and_search_index/grid_index",
                "unified_grid_and_search_index/qmd_aggregator",
                "unified_grid_and_search_index/embedding_map",
                "exports/csv",
            ]
        );
        let check = datalib_dag::config::check_text(&after);
        let applet = &check.cfg.applets[0];
        assert_eq!(applet.id, "unified_grid_and_search_index");
        assert_eq!(
            applet.group.as_deref(),
            Some("unified_grid_and_search_index")
        );
        assert_eq!(
            applet.command,
            "datalib-applet unified_grid_and_search_index"
        );
        assert!(
            after.contains(
                "inputs = [\"picard/render_markdown\", \"unified_grid_and_search_index/grid_index\"]"
            ),
            "{after}"
        );
        assert!(
            after.contains("name = \"Grid and Search Index\""),
            "{after}"
        );
        assert!(
            after.contains("# ── the unified grid and search index ──"),
            "{after}"
        );
    }

    #[test]
    fn renames_the_group_its_steps_their_readers_and_the_applet() {
        assert_renamed(UNIFIED_INDEX_SHAPE);
    }

    /// The name the group had until this build goes the same way as the
    /// one before it, the banner's rule of `─` kept as long as it was.
    #[test]
    fn the_document_index_name_is_renamed_too() {
        let before = document_index_shape();
        assert_renamed(&before);
        let after = rewrite(&before).unwrap();
        assert!(
            after.contains("# ── the unified grid and search index ──────────\n"),
            "{after}"
        );
    }

    #[test]
    fn comments_and_layout_survive() {
        let after = rewrite(UNIFIED_INDEX_SHAPE).unwrap();
        for kept in [
            "# my sources",
            "name = \"Captain's log\"",
            "inputs = [\n  # the map reads the aggregator\n  \"unified_grid_and_search_index/qmd_aggregator\",\n]",
            "# serves the grid",
        ] {
            assert!(after.contains(kept), "lost {kept:?}:\n{after}");
        }
    }

    #[test]
    fn a_current_config_and_a_name_a_person_typed_are_left_alone() {
        let after = rewrite(UNIFIED_INDEX_SHAPE).unwrap();
        assert_eq!(rewrite(&after).unwrap(), after);
        let named =
            UNIFIED_INDEX_SHAPE.replace("name = \"Unified Index\"", "name = \"Everything\"");
        assert!(rewrite(&named).unwrap().contains("name = \"Everything\""));
    }

    /// Only the untyped group is the index: a source that took an old name
    /// keeps it, and so does everything filed under it.
    #[test]
    fn a_source_with_an_old_name_is_not_the_index() {
        for old in OLD {
            let text = format!(
                "[[groups]]\nid = \"{old}\"\ntype = \"email\"\n\n\
                 [[steps]]\ngroup = \"{old}\"\nfunction = \"ingest\"\n"
            );
            assert!(!is_retired(&text).unwrap(), "{text}");
            assert_eq!(rewrite(&text).unwrap(), text);
        }
    }

    #[test]
    fn the_applet_command_keeps_its_path_and_quoting() {
        assert_eq!(
            renamed_command("'/x/bin/datalib_applet' unified_index --flag", &OLD).as_deref(),
            Some("'/x/bin/datalib_applet' unified_grid_and_search_index --flag")
        );
        assert_eq!(
            renamed_command("datalib-applet unified_document_index", &OLD).as_deref(),
            Some("datalib-applet unified_grid_and_search_index")
        );
        assert_eq!(renamed_command("python unified_index.py", &OLD), None);
        assert_eq!(renamed_command("datalib-applet slack", &OLD), None);
    }
}
