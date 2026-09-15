//! Every entry a config file declares, as written — including the ones
//! the loader will drop. The Manage screen shows a row per entry in the
//! *file*, because the file is what a person edits, and a row that
//! reads "Not loaded" is how a dropped entry gets fixed. So this reads
//! leniently: an entry is listed if it is a table with an id, whatever
//! else is wrong with it. `config::check_text` is what says whether it
//! runs.

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WrittenGroup {
    pub id: String,
    pub name: Option<String>,
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WrittenStep {
    /// `<group>/<function>` when both are written, else the written
    /// `id`. The loader composes it the same way.
    pub id: String,
    pub group: Option<String>,
    pub function: Option<String>,
    /// The step's own `name =`, trimmed; blank counts as absent.
    pub name: Option<String>,
    pub inputs: Vec<String>,
    /// The step's `params`, as JSON. What the row's client-side
    /// decoration reads an ingest method off.
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WrittenApplet {
    pub id: String,
    pub group: Option<String>,
    /// The word after `datalib-applet` in its command, when it is one;
    /// the same shape as a source's type, and what names the applet.
    pub r#type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct WrittenEntries {
    pub groups: Vec<WrittenGroup>,
    pub steps: Vec<WrittenStep>,
    pub applets: Vec<WrittenApplet>,
}

/// `Err` only when the text is not TOML at all; anything that parses
/// lists whatever entries it can name.
pub fn entries_as_written(text: &str) -> Result<WrittenEntries, String> {
    let root: toml::Value = toml::from_str(text).map_err(|e| e.message().trim().to_string())?;
    let tables = |key: &str| -> Vec<&toml::Value> {
        root.get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter(|v| v.is_table()).collect())
            .unwrap_or_default()
    };
    let s = |v: &toml::Value, k: &str| -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(str::to_string)
    };
    let nonblank = |v: Option<String>| v.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());

    let groups = tables("groups")
        .into_iter()
        .filter_map(|v| {
            Some(WrittenGroup {
                id: s(v, "id")?,
                name: nonblank(s(v, "name")),
                r#type: s(v, "type"),
            })
        })
        .collect();

    let steps = tables("steps")
        .into_iter()
        .filter_map(|v| {
            let group = s(v, "group");
            let function = s(v, "function");
            let id = match (&group, &function) {
                (Some(g), Some(f)) => format!("{g}/{f}"),
                _ => s(v, "id")?,
            };
            let inputs = v
                .get("inputs")
                .and_then(|x| x.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let params = v
                .get("params")
                .and_then(|p| serde_json::to_value(p).ok())
                .unwrap_or(serde_json::Value::Object(Default::default()));
            Some(WrittenStep {
                id,
                group,
                function,
                name: nonblank(s(v, "name")),
                inputs,
                params,
            })
        })
        .collect();

    let applets = tables("applets")
        .into_iter()
        .filter_map(|v| {
            Some(WrittenApplet {
                id: s(v, "id")?,
                group: s(v, "group"),
                r#type: s(v, "command").as_deref().and_then(applet_type),
            })
        })
        .collect();

    Ok(WrittenEntries {
        groups,
        steps,
        applets,
    })
}

/// `datalib-applet unified_index` → `unified_index`. None for anything
/// else, which is legitimate — an applet may be any executable.
fn applet_type(command: &str) -> Option<String> {
    let mut words = command.split_whitespace();
    // A quoted path, as a config written by a tool spells it; the
    // bazel-built binary is `datalib_applet` under its target name.
    let program = words.next()?.trim_matches(|c| c == '\'' || c == '"');
    let word = words.next()?;
    let base = program.rsplit('/').next().unwrap_or(program);
    let is_ours = base == "datalib-applet" || base == "datalib_applet";
    is_ours.then(|| word.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step the loader rejects (an unknown key) is still listed: the
    /// row is how someone finds out it was dropped.
    #[test]
    fn lists_an_entry_the_loader_would_drop() {
        let text = r#"
[[groups]]
id = "slack"
type = "slack"
name = "  "

[[steps]]
group = "slack"
function = "ingest"
title = "not a key"
[steps.params.api]
channels = ["general"]

[[steps]]
id = "custom/one"
inputs = ["slack/ingest", 7]
name = " Custom "

[[applets]]
id = "view"
command = "/opt/bin/datalib-applet unified_index"

[[applets]]
id = "other"
command = "python serve.py"

[[applets]]
id = "built"
command = "'/x/bin/datalib_applet' slack"
"#;
        let got = entries_as_written(text).unwrap();
        assert_eq!(
            got.groups,
            vec![WrittenGroup {
                id: "slack".into(),
                name: None,
                r#type: Some("slack".into())
            }]
        );
        assert_eq!(got.steps.len(), 2);
        assert_eq!(got.steps[0].id, "slack/ingest");
        assert_eq!(got.steps[0].params["api"]["channels"][0], "general");
        assert_eq!(got.steps[1].id, "custom/one");
        assert_eq!(got.steps[1].inputs, vec!["slack/ingest".to_string()]);
        assert_eq!(got.steps[1].name.as_deref(), Some("Custom"));
        assert_eq!(got.applets[0].r#type.as_deref(), Some("unified_index"));
        assert_eq!(got.applets[1].r#type, None);
        assert_eq!(got.applets[2].r#type.as_deref(), Some("slack"));
    }

    #[test]
    fn a_step_with_no_id_at_all_is_skipped_and_bad_toml_is_an_error() {
        let got = entries_as_written("[[steps]]\ngroup = \"x\"\n").unwrap();
        assert!(got.steps.is_empty());
        assert!(entries_as_written("[[steps").is_err());
    }
}
