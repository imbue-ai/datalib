//! The shape from before Todoist's window took the grammar every source
//! shares: its ingest's `api` table named it `completed_since`. The
//! rewrite renames that key to `since`, the value as it was, since every
//! date the old key took the new one takes too.
//!
//! A text edit, like `step_commands.rs`: every comment and key the file
//! holds survives.

use std::collections::HashSet;

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

const OLD: &str = "completed_since";
const NEW: &str = "since";

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
    #[serde(default)]
    r#type: Option<String>,
}

#[derive(Deserialize)]
struct StepView {
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    params: Option<ParamsView>,
}

#[derive(Deserialize)]
struct ParamsView {
    #[serde(default)]
    api: Option<ApiView>,
}

#[derive(Deserialize)]
struct ApiView {
    #[serde(default)]
    completed_since: Option<Spanned<toml::Value>>,
}

/// Where each Todoist ingest's `completed_since` key starts.
fn keys(text: &str) -> Result<Vec<usize>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let todoist: HashSet<&str> = view
        .groups
        .iter()
        .filter(|g| g.r#type.as_deref() == Some("todoist"))
        .map(|g| g.id.as_str())
        .collect();
    Ok(view
        .steps
        .iter()
        .filter(|s| s.group.as_deref().is_some_and(|g| todoist.contains(g)))
        .filter(|s| s.function.as_deref() == Some("ingest"))
        .filter_map(|s| s.params.as_ref()?.api.as_ref()?.completed_since.as_ref())
        .filter_map(|v| {
            let before = &text[..v.span().start];
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            before[line_start..].rfind(OLD).map(|i| line_start + i)
        })
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!keys(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut at = keys(text)?;
    // Renaming the last key first leaves every earlier one where it was.
    at.sort_unstable_by(|a, b| b.cmp(a));
    let mut out = text.to_string();
    for i in at {
        out.replace_range(i..i + OLD.len(), NEW);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "tasks"
type = "todoist"

[[groups]]
id = "notes"
type = "notion"

[[steps]]
group = "tasks"
function = "ingest"
command = "datalib-step"
[steps.params.api]
completed = true
completed_since = "2364-01-01"  # the first day of the mission

[[steps]]
group = "notes"
function = "ingest"
command = "datalib-step"
params = { api = { completed_since = "kept: not a todoist group" } }
"#;

    #[test]
    fn a_todoist_ingests_completed_since_becomes_since_and_nothing_else_moves() {
        let out = rewrite(BEFORE).unwrap();
        assert_eq!(
            out,
            BEFORE.replace("completed_since = \"2364-01-01\"", "since = \"2364-01-01\"")
        );
        assert!(!is_retired(&out).unwrap());
    }

    #[test]
    fn an_inline_api_table_is_renamed_too() {
        let inline = "[[groups]]\nid = \"t\"\ntype = \"todoist\"\n\n[[steps]]\ngroup = \"t\"\n\
                      function = \"ingest\"\nparams = { api = { completed_since = \"2364-01-01\" } }\n";
        assert_eq!(
            rewrite(inline).unwrap(),
            inline.replace("completed_since", "since")
        );
    }
}
