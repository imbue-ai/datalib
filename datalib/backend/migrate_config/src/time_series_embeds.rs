//! A config whose time-series sources (`datalib_dag::config::TIME_SERIES_SOURCE_TYPES`)
//! still have an `embed` step. Their markdown is daily summaries of
//! numbers, which the keyword index already finds, and embedding it costs
//! minutes and gigabytes a run; the wizard no longer offers it
//! (`embedAllowed` in `ui/src/config/sourceSteps.ts`). The rewrite takes
//! each such step out, with its comments, and takes its id out of every
//! `inputs` that names it. Everything else stays as written.

use std::collections::BTreeSet;
use std::ops::Range;

use anyhow::{Context as _, Result};
use datalib_dag::config::TIME_SERIES_SOURCE_TYPES;
use datalib_dag::config_array::edit_string_array;
use datalib_dag::config_order::entry_ranges;
use serde::Deserialize;

const EMBED: &str = "embed";

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
    command: Option<String>,
    #[serde(default)]
    inputs: Option<toml::Spanned<Vec<String>>>,
}

impl StepView {
    /// The id of this step when it is a built-in `embed` under one of
    /// `groups`.
    fn embed_under(&self, groups: &BTreeSet<&str>) -> Option<String> {
        let group = self.group.as_deref()?;
        (self.command.is_none()
            && self.function.as_deref() == Some(EMBED)
            && groups.contains(group))
        .then(|| format!("{group}/{EMBED}"))
    }
}

/// The ids of the `embed` steps the rewrite takes out.
fn embeds(view: &View) -> BTreeSet<String> {
    let groups: BTreeSet<&str> = view
        .groups
        .iter()
        .filter(|g| {
            g.r#type
                .as_deref()
                .is_some_and(|t| TIME_SERIES_SOURCE_TYPES.contains(&t))
        })
        .map(|g| g.id.as_str())
        .collect();
    view.steps
        .iter()
        .filter_map(|s| s.embed_under(&groups))
        .collect()
}

pub fn removed(text: &str) -> Result<BTreeSet<String>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    Ok(embeds(&view))
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!removed(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let gone = embeds(&view);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    for range in entry_ranges(text) {
        if is_one_of(&text[range.clone()], &gone) {
            edits.push((range, String::new()));
        }
    }
    for inputs in view.steps.iter().filter_map(|s| s.inputs.as_ref()) {
        if inputs.get_ref().iter().any(|i| gone.contains(i)) {
            let kept: Vec<String> = inputs
                .get_ref()
                .iter()
                .filter(|i| !gone.contains(*i))
                .cloned()
                .collect();
            edits.push((
                inputs.span(),
                edit_string_array(&text[inputs.span()], &kept).map_err(anyhow::Error::msg)?,
            ));
        }
    }
    // Spans index the original text, so replacing the last first leaves
    // every earlier span where it was.
    edits.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for (span, with) in edits {
        out.replace_range(span, &with);
    }
    Ok(out)
}

/// Whether this entry of the file is a built-in step whose id is in
/// `ids`.
fn is_one_of(entry: &str, ids: &BTreeSet<String>) -> bool {
    let Ok(table) = toml::from_str::<toml::Table>(entry) else {
        return false;
    };
    let Some(step) = table
        .get("steps")
        .and_then(toml::Value::as_array)
        .and_then(|a| a.first())
    else {
        return false;
    };
    let key = |k: &str| step.get(k).and_then(toml::Value::as_str);
    match (key("group"), key("function"), key("command")) {
        (Some(group), Some(function), None) => ids.contains(&format!("{group}/{function}")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"# the away team's watches
[[groups]]
id = "riker"
type = "garmin"

[[steps]]
group = "riker"
function = "ingest"

[[steps]]
group = "riker"
function = "render_markdown"
inputs = ["riker/ingest"]

[[steps]]
group = "riker"
function = "keyword_index"
inputs = ["riker/render_markdown"]

# riker's embeddings
[[steps]]
group = "riker"
function = "embed"
inputs = ["riker/keyword_index"]

[[groups]]
id = "troi-mail"
type = "email"

[[steps]]
group = "troi-mail"
function = "ingest"
[steps.params.mbox]
path = "/deck8/troi.mbox"

[[steps]]
group = "troi-mail"
function = "render_markdown"
inputs = ["troi-mail/ingest"]

[[steps]]
group = "troi-mail"
function = "keyword_index"
inputs = ["troi-mail/render_markdown"]

[[steps]]
group = "troi-mail"
function = "embed"
inputs = ["troi-mail/keyword_index"]

[[groups]]
id = "unified_document_index"

[[steps]]
group = "unified_document_index"
function = "qmd_aggregator"
inputs = [
    "riker/keyword_index",
    "riker/embed",
    "troi-mail/keyword_index",
    "troi-mail/embed",
]
"#;

    fn step_ids(text: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(text);
        assert_eq!(
            check.dropped(),
            0,
            "{}",
            check.render("config.toml".as_ref())
        );
        check.cfg.steps.iter().map(|s| s.id.clone()).collect()
    }

    /// The garmin source's embed step goes, comment and all, and the
    /// aggregator stops naming it; the email source keeps its own.
    #[test]
    fn a_time_series_embed_step_is_taken_out() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        let ids = step_ids(&after);
        assert!(!ids.contains(&"riker/embed".to_string()), "{after}");
        assert!(ids.contains(&"troi-mail/embed".to_string()), "{after}");
        assert!(ids.contains(&"unified_document_index/qmd_aggregator".to_string()));
        assert!(!after.contains("riker's embeddings"), "{after}");
        assert!(
            after.contains(
                "inputs = [\n    \"riker/keyword_index\",\n    \"troi-mail/keyword_index\",\n    \"troi-mail/embed\",\n]"
            ),
            "{after}"
        );
        assert!(after.starts_with("# the away team's watches"), "{after}");
        assert!(!is_retired(&after).unwrap());
    }

    #[test]
    fn a_config_with_no_time_series_embed_is_left_alone() {
        let after = rewrite(BEFORE).unwrap();
        assert_eq!(rewrite(&after).unwrap(), after);
    }
}
