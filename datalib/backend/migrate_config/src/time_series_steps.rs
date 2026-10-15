//! A config from before time series: its sources of a type that renders
//! them (`datalib_dag::config::TIME_SERIES_SOURCE_TYPES`) have no
//! `render_time_series` step, and there is no time-series index. The
//! rewrite adds the steps, the index group with its `time_series_index`
//! and the applet that serves it, appended after everything the file
//! holds, so every comment and entry stays where it was.
//!
//! It applies only to a config with no `unified_time_series_index` group.
//! Once that group is there the time-series steps are the person's: a
//! `render_time_series` step they took out is not put back.

use anyhow::{Context as _, Result};
use datalib_dag::config::{TIME_SERIES_INDEX_GROUP, TIME_SERIES_SOURCE_TYPES};
use serde::Deserialize;

const RENDER: &str = "render_time_series";
const INDEX: &str = "time_series_index";
const INGEST: &str = "ingest";

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
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

/// The sources that get a `render_time_series` step, in file order:
/// typed for time series and with an ingest step to read. Empty when the
/// config already has a time-series index, or needs none.
fn sources(text: &str) -> Result<Vec<String>> {
    let view: View = toml::from_str(text).context("parse the config")?;
    if view.groups.iter().any(|g| g.id == TIME_SERIES_INDEX_GROUP) {
        return Ok(Vec::new());
    }
    let has = |group: &str, function: &str| {
        view.steps
            .iter()
            .any(|s| s.group.as_deref() == Some(group) && s.function.as_deref() == Some(function))
    };
    Ok(view
        .groups
        .iter()
        .filter(|g| {
            g.r#type
                .as_deref()
                .is_some_and(|t| TIME_SERIES_SOURCE_TYPES.contains(&t))
        })
        .filter(|g| has(&g.id, INGEST))
        .map(|g| g.id.clone())
        .collect())
}

pub fn is_retired(text: &str) -> Result<bool> {
    Ok(!sources(text)?.is_empty())
}

pub fn rewrite(text: &str) -> Result<String> {
    let sources = sources(text)?;
    let mut out = text.trim_end().to_string();
    for source in &sources {
        out.push_str(&format!(
            "\n\n[[steps]]\ngroup = {}\nfunction = {}\ninputs = [{}]",
            quote(source),
            quote(RENDER),
            quote(&format!("{source}/{INGEST}")),
        ));
    }
    let inputs: Vec<String> = sources
        .iter()
        .map(|s| quote(&format!("{s}/{RENDER}")))
        .collect();
    out.push_str(&format!(
        "\n\n# ── the time-series index: every source's time series, stacked ──\n\
         [[groups]]\nid = {group}\nname = \"Time Series Index\"\n\n\
         [[steps]]\ngroup = {group}\nfunction = {}\ninputs = [{}]\n\n\
         [[applets]]\ngroup = {group}\nid = {group}\ncommand = {}\n",
        quote(INDEX),
        inputs.join(", "),
        quote(&format!("datalib-applet {TIME_SERIES_INDEX_GROUP}")),
        group = quote(TIME_SERIES_INDEX_GROUP),
    ));
    Ok(out)
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

[[groups]]
id = "troi-mail"
type = "email"

[[steps]]
group = "troi-mail"
function = "ingest"
[steps.params.mbox]
path = "/deck8/troi.mbox"
"#;

    fn step_ids(text: &str) -> Vec<String> {
        let check = datalib_dag::config::check_text(text);
        check.cfg.steps.iter().map(|s| s.id.clone()).collect()
    }

    #[test]
    fn a_time_series_source_gets_its_step_and_the_index_is_added() {
        assert!(is_retired(BEFORE).unwrap());
        let after = rewrite(BEFORE).unwrap();
        assert!(after.starts_with(BEFORE.trim_end()), "{after}");
        let ids = step_ids(&after);
        assert!(
            ids.contains(&"riker/render_time_series".to_string()),
            "{ids:?}"
        );
        assert!(
            ids.contains(&"unified_time_series_index/time_series_index".to_string()),
            "{ids:?}"
        );
        assert!(!ids.iter().any(|i| i.starts_with("troi-mail/render_time")));
        assert!(after.contains(r#"inputs = ["riker/render_time_series"]"#));
        let check = datalib_dag::config::check_text(&after);
        let applets: Vec<&str> = check.cfg.applets.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(applets, ["unified_time_series_index"]);
        assert!(!is_retired(&after).unwrap());
    }

    /// Once the index group is there, a step the person took out stays out.
    #[test]
    fn a_config_with_the_index_group_is_left_alone() {
        let removed = format!(
            "{BEFORE}\n[[groups]]\nid = \"unified_time_series_index\"\n\n\
             [[steps]]\ngroup = \"unified_time_series_index\"\nfunction = \"time_series_index\"\ninputs = []\n"
        );
        assert!(!is_retired(&removed).unwrap());
    }

    #[test]
    fn a_config_with_no_time_series_source_is_left_alone() {
        let mail_only = BEFORE
            .split("[[groups]]\nid = \"troi-mail\"")
            .nth(1)
            .unwrap();
        let mail_only = format!("[[groups]]\nid = \"troi-mail\"{mail_only}");
        assert!(!is_retired(&mail_only).unwrap());
    }
}
