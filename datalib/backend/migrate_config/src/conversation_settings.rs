//! The shape from before the ChatGPT and Claude sources named their API
//! settings alike. ChatGPT's ingest called them `max_pages`, `limit` and
//! `sleep_between`, and Claude's called its re-fetch count
//! `refresh_most_recent_n_chat_count`. The rewrite gives each key its
//! current name, the value as it was: each means what it meant.
//!
//! A text edit, like `todoist_since.rs`: every comment and key the file
//! holds survives.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context as _, Result};
use serde::Deserialize;
use toml::Spanned;

/// Each source type's renamed `api` keys, old then new.
const RENAMES: &[(&str, &str, &str)] = &[
    ("chatgpt", "max_pages", "max_listing_pages"),
    ("chatgpt", "limit", "max_conversations_per_run"),
    ("chatgpt", "sleep_between", "seconds_between_requests"),
    (
        "claude",
        "refresh_most_recent_n_chat_count",
        "refetch_newest_conversations",
    ),
];

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
    api: Option<BTreeMap<String, Spanned<toml::Value>>>,
}

/// Where each old key starts, and what it becomes.
fn keys(text: &str) -> Result<Vec<(usize, &'static str, &'static str)>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let type_of: HashMap<&str, &str> = view
        .groups
        .iter()
        .filter_map(|g| Some((g.id.as_str(), g.r#type.as_deref()?)))
        .collect();
    let mut at = Vec::new();
    for step in &view.steps {
        if step.function.as_deref() != Some("ingest") {
            continue;
        }
        let Some(ty) = step.group.as_deref().and_then(|g| type_of.get(g)) else {
            continue;
        };
        let Some(api) = step.params.as_ref().and_then(|p| p.api.as_ref()) else {
            continue;
        };
        for &(_, old, new) in RENAMES.iter().filter(|(t, _, _)| t == ty) {
            let Some(value) = api.get(old) else {
                continue;
            };
            let before = &text[..value.span().start];
            let line_start = before.rfind('\n').map_or(0, |i| i + 1);
            if let Some(i) = before[line_start..].rfind(old) {
                at.push((line_start + i, old, new));
            }
        }
    }
    Ok(at)
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!keys(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut at = keys(text)?;
    // Renaming the last key first leaves every earlier one where it was.
    at.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    let mut out = text.to_string();
    for (i, old, new) in at {
        out.replace_range(i..i + old.len(), new);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "gpt"
type = "chatgpt"

[[groups]]
id = "anthropic"
type = "claude"

[[groups]]
id = "notes"
type = "notion"

[[steps]]
group = "gpt"
function = "ingest"
command = "datalib-step"
[steps.params.api]
since = "2364-01-01"
max_pages = 20       # pages of 100
limit = 100
sleep_between = 0.5

[[steps]]
group = "anthropic"
function = "ingest"
command = "datalib-step"
params = { api = { refresh_most_recent_n_chat_count = 3, projects = false } }

[[steps]]
group = "notes"
function = "ingest"
command = "datalib-step"
params = { api = { max_pages = 7 } }
"#;

    #[test]
    fn each_old_key_takes_its_new_name_and_nothing_else_moves() {
        let out = rewrite(BEFORE).unwrap();
        let want = BEFORE
            .replace("max_pages = 20", "max_listing_pages = 20")
            .replace("limit = 100", "max_conversations_per_run = 100")
            .replace("sleep_between = 0.5", "seconds_between_requests = 0.5")
            .replace(
                "refresh_most_recent_n_chat_count = 3",
                "refetch_newest_conversations = 3",
            );
        assert_eq!(out, want);
        assert!(out.contains("{ max_pages = 7 }"), "notion's own key stays");
        assert!(!is_retired(&out).unwrap());
    }

    /// Two old keys on one line, in an inline table, are each renamed.
    #[test]
    fn two_keys_on_one_line_are_both_renamed() {
        let inline = "[[groups]]\nid = \"g\"\ntype = \"chatgpt\"\n\n[[steps]]\ngroup = \"g\"\n\
                      function = \"ingest\"\nparams = { api = { limit = 5, max_pages = 2 } }\n";
        assert_eq!(
            rewrite(inline).unwrap(),
            inline.replace(
                "{ limit = 5, max_pages = 2 }",
                "{ max_conversations_per_run = 5, max_listing_pages = 2 }"
            )
        );
    }
}
