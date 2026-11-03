//! The shape from before ids that name folders had to be lower case: a
//! group, step or applet id, or a step's function, could hold capitals,
//! and on a Mac's disk, which ignores letter case, `Email/` and `email/`
//! are one folder. The rewrite lowercases every such id and every place
//! that names one: a diff group's `source`, a step's `group`, `function`
//! and `inputs`, an applet's `group`. The folders follow at boot
//! (`datalib-http`'s `config_upgrade::move_case_folders`).
//!
//! A text edit, like `document_index.rs`: values are replaced where they
//! stand, so every comment and key the file holds survives.

use std::ops::Range;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

use datalib_dag::config_array::edit_string_array;

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
    source: Option<Spanned<String>>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    id: Option<Spanned<String>>,
    #[serde(default)]
    group: Option<Spanned<String>>,
    #[serde(default)]
    function: Option<Spanned<String>>,
    #[serde(default)]
    inputs: Option<Spanned<Vec<String>>>,
}

#[derive(Deserialize)]
struct AppletView {
    #[serde(default)]
    id: Option<Spanned<String>>,
    #[serde(default)]
    group: Option<Spanned<String>>,
}

/// `id` in lower case; `None` when it already is.
pub fn renamed(id: &str) -> Option<String> {
    id.chars()
        .any(|c| c.is_ascii_uppercase())
        .then(|| id.to_ascii_lowercase())
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// `written` is one whole array as the file has it; each entry `after`
/// renames is swapped where it stands, and the read-back check makes the
/// result spell exactly `after`.
fn rename_in_array(written: &str, before: &[String], after: &[String]) -> Result<String, String> {
    let mut out = written.to_string();
    for (old, new) in before.iter().zip(after) {
        if old == new {
            continue;
        }
        let token = quote(old);
        if let Some(at) = out.find(&token) {
            out.replace_range(at..at + token.len(), &quote(new));
        }
    }
    edit_string_array(&out, after)
}

fn edits(text: &str) -> Result<Vec<(Range<usize>, String)>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let names = view
        .groups
        .iter()
        .flat_map(|g| std::iter::once(&g.id).chain(&g.source))
        .chain(
            view.steps
                .iter()
                .flat_map(|s| s.id.iter().chain(&s.group).chain(&s.function)),
        )
        .chain(
            view.applets
                .iter()
                .flat_map(|a| a.id.iter().chain(&a.group)),
        );
    let mut out: Vec<(Range<usize>, String)> = names
        .filter_map(|s| renamed(s.get_ref()).map(|new| (s.span(), quote(&new))))
        .collect();
    for i in view.steps.iter().filter_map(|s| s.inputs.as_ref()) {
        let before = i.get_ref();
        if before.iter().any(|x| renamed(x).is_some()) {
            let after: Vec<String> = before
                .iter()
                .map(|x| renamed(x).unwrap_or_else(|| x.clone()))
                .collect();
            out.push((
                i.span(),
                rename_in_array(&text[i.span()], before, &after).map_err(anyhow::Error::msg)?,
            ));
        }
    }
    Ok(out)
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

    const MIXED: &str = r#"# my sources
[[groups]]
id = "Email"   # the work account
type = "email"

[[groups]]
id = "Email-Diff"
type = "diff"
source = "Email"

[[steps]]
group = "Email"
function = "ingest"

[[steps]]
id = "Custom/Raw"
command = "fetch-it"

[[steps]]
group = "unified_document_index"
function = "grid_index"
inputs = ["Email/render_markdown", "Custom/Raw"]

[[applets]]
id = "Photos"
group = "Email"
command = "datalib-applet photos"
"#;

    #[test]
    fn lowercases_every_folder_id_and_every_place_that_names_one() {
        let out = rewrite(MIXED).unwrap();
        for upper in [
            "\"Email\"",
            "Email-Diff",
            "Custom/Raw",
            "Email/render",
            "\"Photos\"",
        ] {
            assert!(!out.contains(upper), "{upper} left in:\n{out}");
        }
        for lower in [
            r#"id = "email"   # the work account"#,
            r#"source = "email""#,
            r#"id = "custom/raw""#,
            r#"inputs = ["email/render_markdown", "custom/raw"]"#,
            r#"id = "photos""#,
        ] {
            assert!(out.contains(lower), "{lower} missing from:\n{out}");
        }
        assert!(out.starts_with("# my sources\n"), "the comment survives");
        assert!(out.contains(r#"command = "datalib-applet photos""#));
    }

    #[test]
    fn a_config_already_in_lower_case_is_left_alone() {
        let lower = rewrite(MIXED).unwrap();
        assert!(!is_retired(&lower).unwrap());
        assert!(is_retired(MIXED).unwrap());
    }
}
