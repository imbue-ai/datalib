//! The one rewrite: ungrouped `[[steps]]` into `[[groups]]` plus
//! `group` + `function` steps.
//!
//! Value-level: the config is parsed, regrouped and serialized again, so
//! comments and formatting do not survive. The output says so at the top.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use datalib_dag::config::{AppletEntry, DagConfig, StepEntry};
use serde::Serialize;

/// Whether anything in this config is a `datalib-step` step written the
/// old way. A config of only custom steps, or one already grouped, has
/// nothing for this rewrite to do.
pub fn needs_grouping(cfg: &DagConfig) -> bool {
    cfg.steps
        .iter()
        .any(|s| s.group.is_none() && builtin_of(s).is_some())
}

/// What an old-style `datalib-step` step declared, read off its id and
/// its command together.
struct Builtin {
    group: String,
    function: &'static str,
    r#type: Option<String>,
}

fn builtin_of(step: &StepEntry) -> Option<Builtin> {
    let words: Vec<&str> = step.command.split_whitespace().collect();
    let prog = words.first()?;
    if !(*prog == "datalib-step" || prog.ends_with("/datalib-step")) {
        return None;
    }
    let (stem, leaf) = step.id.split_once('/')?;
    if leaf.contains('/') {
        return None;
    }
    let typed = |function: &'static str| {
        Some(Builtin {
            group: stem.to_string(),
            function,
            r#type: Some(words.get(2)?.to_string()),
        })
    };
    let index = |function: &'static str| {
        (stem == "unified_index").then(|| Builtin {
            group: stem.to_string(),
            function,
            r#type: None,
        })
    };
    match (words.get(1).copied(), leaf) {
        (Some("download"), "raw") => typed("raw"),
        (Some("render"), "rendered_md") => typed("rendered_md"),
        (Some("grid_index"), "grid") => index("grid"),
        (Some("qmd_index"), "qmd") => index("qmd"),
        _ => None,
    }
}

#[derive(Serialize)]
struct GroupOut {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#type: Option<String>,
}

/// One step, shaped for output: `group` + `function` or `id`, never both,
/// and `params` last so its `[steps.params.…]` headers land after the plain
/// keys — a table header ends the table it appears in.
#[derive(Serialize)]
struct StepOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<toml::Value>,
}

#[derive(Serialize)]
struct AppletOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    id: String,
    command: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<toml::Value>,
}

/// One `[[<key>]]` block. Serialized straight from the struct rather than
/// through a `toml::Table`, which is a sorted map and would put `function`
/// before `group` and `command` before both.
fn block<T: Serialize>(key: &str, entry: &T) -> Result<String> {
    #[derive(Serialize)]
    struct Groups<'a, T> {
        groups: [&'a T; 1],
    }
    #[derive(Serialize)]
    struct Steps<'a, T> {
        steps: [&'a T; 1],
    }
    #[derive(Serialize)]
    struct Applets<'a, T> {
        applets: [&'a T; 1],
    }
    let text = match key {
        "groups" => toml::to_string(&Groups { groups: [entry] }),
        "steps" => toml::to_string(&Steps { steps: [entry] }),
        "applets" => toml::to_string(&Applets { applets: [entry] }),
        other => bail!("no [[{other}]] array in a config"),
    };
    text.with_context(|| format!("serialize a [[{key}]] entry"))
}

pub fn group_steps(text: &str) -> Result<String> {
    let cfg = datalib_dag::config::parse(text).context("parse the config to rewrite")?;

    // Groups in the order their first step appears, so the output reads the
    // way the input did.
    let mut groups: Vec<GroupOut> = Vec::new();
    let mut steps: Vec<(Option<usize>, StepOut)> = Vec::with_capacity(cfg.steps.len());
    for step in &cfg.steps {
        let Some(b) = step.group.is_none().then(|| builtin_of(step)).flatten() else {
            steps.push((
                None,
                StepOut {
                    group: step.group.clone(),
                    function: step.function.clone(),
                    id: step.group.is_none().then(|| step.id.clone()),
                    name: step.name.clone(),
                    command: step.command.clone(),
                    inputs: step.inputs.clone(),
                    env: step.env.clone(),
                    code_version: step.code_version.clone(),
                    params: step.params.clone(),
                },
            ));
            continue;
        };
        let gi = match groups.iter().position(|g| g.id == b.group) {
            Some(i) => i,
            None => {
                groups.push(GroupOut {
                    id: b.group.clone(),
                    name: None,
                    r#type: None,
                });
                groups.len() - 1
            }
        };
        let group = &mut groups[gi];
        match (&group.r#type, &b.r#type) {
            (Some(have), Some(want)) if have != want => bail!(
                "steps under {:?} disagree about its type: {have:?} and {want:?}",
                b.group
            ),
            (None, Some(want)) => group.r#type = Some(want.clone()),
            _ => {}
        }
        // The download step's name is the source's name; a render step's is
        // the same thing said again, and the group carries it once.
        if group.name.is_none() {
            group.name = step.name.clone();
        }
        steps.push((
            Some(gi),
            StepOut {
                group: Some(b.group),
                function: Some(b.function.to_string()),
                id: None,
                name: None,
                command: step.command.clone(),
                inputs: step.inputs.clone(),
                env: step.env.clone(),
                code_version: step.code_version.clone(),
                params: step.params.clone(),
            },
        ));
    }

    let applets: Vec<AppletOut> = cfg
        .applets
        .iter()
        .map(|a| AppletOut {
            group: a.group.clone().or_else(|| applet_group(a, &groups)),
            id: a.id.clone(),
            command: a.command.clone(),
            env: a.env.clone(),
            params: a.params.clone(),
        })
        .collect();

    let mut out = String::from(
        "# Rewritten by datalib-migrate-config into the [[groups]] shape.\n\
         # Comments and formatting from the previous file are not carried\n\
         # over; review before relying on it.\n\n",
    );
    out.push_str(&header(&cfg)?);
    let mut emitted = vec![false; groups.len()];
    for (gi, step) in &steps {
        out.push('\n');
        if let Some(gi) = *gi {
            if !emitted[gi] {
                emitted[gi] = true;
                out.push_str(&block("groups", &groups[gi])?);
                out.push('\n');
            }
        }
        out.push_str(&block("steps", step)?);
    }
    for a in &applets {
        out.push('\n');
        out.push_str(&block("applets", a)?);
    }
    Ok(out)
}

/// The group an applet is filed under: the tree its `params.tree` names,
/// else a group sharing its id — which is how the `unified_index` applet
/// lands beside the index steps.
fn applet_group(a: &AppletEntry, groups: &[GroupOut]) -> Option<String> {
    let known = |id: &str| groups.iter().any(|g| g.id == id).then(|| id.to_string());
    let from_tree = a
        .params
        .as_ref()
        .and_then(|p| p.get("tree"))
        .and_then(|t| t.as_str())
        .and_then(|t| t.split('/').next())
        .and_then(known);
    from_tree.or_else(|| known(&a.id))
}

fn header(cfg: &DagConfig) -> Result<String> {
    #[derive(Serialize)]
    struct Head {
        #[serde(skip_serializing_if = "Option::is_none")]
        data_root: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        binary_dir: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkpoint_cadence: Option<datalib_dag::config::CheckpointCadence>,
    }
    toml::to_string(&Head {
        data_root: cfg.data_root.as_ref().map(|p| p.display().to_string()),
        binary_dir: cfg.binary_dir.as_ref().map(|p| p.display().to_string()),
        checkpoint_cadence: cfg.checkpoint_cadence,
    })
    .context("serialize the top-level keys")
}
