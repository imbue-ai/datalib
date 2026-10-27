//! An email source whose `jmap` or `gmail` table still says
//! `full_resync`. The key is gone: a cursor upstream no longer honours
//! lists the account again by itself, and a person who wants to start
//! over resets the step. Its params no longer load with it, so the
//! rewrite takes the line out, comment and all, and leaves everything
//! else as written.

use std::ops::Range;

use anyhow::{bail, Context as _, Result};
use serde::Deserialize;

const KEY: &str = "full_resync";

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
    command: Option<String>,
    #[serde(default)]
    params: Option<ParamsView>,
}

#[derive(Deserialize)]
struct ParamsView {
    #[serde(default)]
    jmap: Option<MethodView>,
    #[serde(default)]
    gmail: Option<MethodView>,
}

#[derive(Deserialize)]
struct MethodView {
    #[serde(default)]
    full_resync: Option<toml::Spanned<toml::Value>>,
}

/// Where each `full_resync` value of an email source's built-in step
/// stands in the text.
fn value_spans(text: &str) -> Result<Vec<Range<usize>>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    let email_groups: Vec<&str> = view
        .groups
        .iter()
        .filter(|g| g.r#type.as_deref() == Some("email"))
        .map(|g| g.id.as_str())
        .collect();
    Ok(view
        .steps
        .iter()
        .filter(|s| s.command.is_none())
        .filter(|s| {
            s.group
                .as_deref()
                .is_some_and(|g| email_groups.contains(&g))
        })
        .filter_map(|s| s.params.as_ref())
        .flat_map(|p| [p.jmap.as_ref(), p.gmail.as_ref()])
        .flatten()
        .filter_map(|m| m.full_resync.as_ref())
        .map(toml::Spanned::span)
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!value_spans(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let mut lines: Vec<Range<usize>> = value_spans(text)?
        .into_iter()
        .map(|value| whole_line(text, value))
        .collect::<Result<_>>()?;
    // Spans index the original text, so removing the last first leaves
    // every earlier span where it was.
    lines.sort_by_key(|r| std::cmp::Reverse(r.start));
    let mut out = text.to_string();
    for line in lines {
        out.replace_range(line, "");
    }
    Ok(out)
}

/// The line `full_resync = <value>` stands on, its newline included.
/// Anything else (an inline table, a dotted key) is refused: those are
/// written by hand, and a hand is better placed to take the key out.
fn whole_line(text: &str, value: Range<usize>) -> Result<Range<usize>> {
    let start = text[..value.start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[value.end..]
        .find('\n')
        .map_or(text.len(), |i| value.end + i + 1);
    let before = text[start..value.start].trim();
    let after = text[value.end..end].trim();
    let alone = before
        .strip_prefix(KEY)
        .is_some_and(|rest| rest.trim() == "=")
        && (after.is_empty() || after.starts_with('#'));
    if !alone {
        bail!("`{KEY}` is not on a line of its own; take it out by hand");
    }
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEFORE: &str = r#"[[groups]]
id = "enterprise-mail"
type = "email"

[[steps]]
group = "enterprise-mail"
function = "ingest"
[steps.params.jmap]
hostname = "api.fastmail.com"
full_resync = true             # list and fetch every email again
blob_download_concurrency = 8

[[groups]]
id = "picard-gmail"
type = "email"

[[steps]]
group = "picard-gmail"
function = "ingest"
[steps.params.gmail]
full_resync = false
"#;

    #[test]
    fn the_key_goes_from_both_method_tables() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(!after.contains(KEY), "{after}");
        assert!(
            after.contains("hostname = \"api.fastmail.com\"\nblob_download_concurrency = 8\n"),
            "{after}"
        );
        assert!(after.ends_with("[steps.params.gmail]\n"), "{after}");
        assert!(!is_retired(&after).unwrap());
    }

    #[test]
    fn another_source_type_and_a_custom_step_are_left_alone() {
        let text = r#"[[groups]]
id = "data"
type = "slack"

[[steps]]
group = "data"
function = "ingest"
[steps.params.jmap]
full_resync = true

[[groups]]
id = "lore"
type = "email"

[[steps]]
group = "lore"
function = "ingest"
command = "lore-mail"
[steps.params.gmail]
full_resync = true
"#;
        assert!(!is_retired(text).unwrap());
    }

    #[test]
    fn an_inline_table_is_refused() {
        let text = r#"[[groups]]
id = "worf-mail"
type = "email"

[[steps]]
group = "worf-mail"
function = "ingest"
params = { gmail = { full_resync = true } }
"#;
        assert!(is_retired(text).unwrap());
        assert!(rewrite(text).is_err());
    }
}
