//! The one rewrite, from any earlier shape to the current one:
//!
//! - steps that named their function and provider on a `datalib-step
//!   download|render|grid_index|qmd_index …` command line — grouped or
//!   not — become `[[groups]]` plus `group` + `function` steps with no
//!   command, under the function names the trees are now called by;
//! - a group's `type` names the thing mirrored (`slack`, `claude`,
//!   `contacts`), not the way it was reached (`slack_api`,
//!   `claude_export`, `carddav`);
//! - an ingest step's params hold one table per method, named for the
//!   method, and a file-backed method carries its own `path` — so `sync`
//!   becomes `api` (or `jmap`, `carddav`, `texts`, `backup`, `github`) and
//!   `common.input_path` becomes `export.path`, `fswalk.path`,
//!   `mbox.path`, …;
//! - `common.raw_path` goes: the store is the step's own tree, which is
//!   the only value the step ever accepted for it.
//!
//! This module parses the retired shapes itself. The runner refuses them,
//! so the loader cannot hand the entries over, and a retired shape should
//! be understood in exactly one place. [`RETIRED_TYPES`] and
//! [`rewrite_ingest_params`] are that place for the type and method words;
//! `datalib-step` only recognises them well enough to name this tool.
//!
//! Value-level: the config is parsed, regrouped and serialized again, so
//! comments and formatting do not survive. The output says so at the top.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

/// The file as the retired shape wrote it. Every entry key is optional
/// here: this parser's job is to recognise the shape, and the loader's
/// verification of the output is what refuses a half-written entry.
#[derive(Debug, Deserialize)]
struct OldConfig {
    #[serde(default)]
    data_root: Option<String>,
    #[serde(default)]
    binary_dir: Option<String>,
    #[serde(default)]
    checkpoint_cadence: Option<datalib_dag::config::CheckpointCadence>,
    #[serde(default)]
    groups: Vec<GroupIn>,
    #[serde(default)]
    steps: Vec<StepIn>,
    #[serde(default)]
    applets: Vec<AppletIn>,
}

#[derive(Debug, Clone, Deserialize)]
struct GroupIn {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct StepIn {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    code_version: Option<String>,
    #[serde(default)]
    params: Option<toml::Value>,
}

impl StepIn {
    /// The id this step had: composed for a grouped step, written for the
    /// rest. `None` for an entry too broken to name.
    fn old_id(&self) -> Option<String> {
        match (&self.group, &self.function, &self.id) {
            (Some(g), Some(f), _) => Some(format!("{g}/{f}")),
            (_, _, Some(id)) => Some(id.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct AppletIn {
    id: String,
    #[serde(default)]
    group: Option<String>,
    command: String,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    params: Option<toml::Value>,
}

fn parse_old(text: &str) -> Result<OldConfig> {
    toml::from_str(text).context("parse the config to rewrite")
}

/// The shapes this rewrite recognises, most retired first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retired {
    /// Steps naming their function and provider on a `datalib-step`
    /// subcommand line.
    StepSubcommands,
    /// A group `type` spelled for the method (`slack_api`), or an
    /// ingest step whose params still say `sync` / `common.input_path`.
    TypesAndMethodTables,
}

/// Which retired shape this config is in, or `None` when it is already
/// current. A config of only custom steps has nothing for this rewrite to
/// do either.
pub fn retired_shape(text: &str) -> Result<Option<Retired>> {
    let cfg = parse_old(text)?;
    if cfg.steps.iter().any(|s| builtin_of(s).is_some()) {
        return Ok(Some(Retired::StepSubcommands));
    }
    let retired_type = cfg
        .groups
        .iter()
        .any(|g| g.r#type.as_deref().is_some_and(|t| rename_type(t) != t));
    let retired_params = cfg.steps.iter().any(|s| {
        s.command.is_none()
            && s.params.as_ref().is_some_and(|p| {
                (s.function.as_deref() == Some("ingest") && has_retired_param_keys(p))
                    || has_raw_path(p)
            })
    });
    Ok((retired_type || retired_params).then_some(Retired::TypesAndMethodTables))
}

fn has_raw_path(params: &toml::Value) -> bool {
    params
        .get("common")
        .and_then(|c| c.get("raw_path"))
        .is_some()
}

/// `common.raw_path` could only ever name the step's own tree; drop it,
/// and `common` with it once empty.
fn strip_raw_path(params: &mut toml::Table) {
    if let Some(common) = params.get_mut("common").and_then(|c| c.as_table_mut()) {
        common.remove("raw_path");
        if common.is_empty() {
            params.remove("common");
        }
    }
}

fn has_retired_param_keys(params: &toml::Value) -> bool {
    params.get("sync").is_some()
        || params
            .get("common")
            .and_then(|c| c.get("input_path"))
            .is_some()
}

/// The type words configs used before a group's `type` named the thing
/// mirrored, and what each is now.
pub const RETIRED_TYPES: &[(&str, &str)] = &[
    ("carddav", "contacts"),
    ("chatgpt_api", "chatgpt"),
    ("claude_api", "claude"),
    ("claude_export", "claude"),
    ("github_api", "github"),
    ("gitlab_api", "gitlab"),
    ("notion_api", "notion"),
    ("signal_backup", "signal"),
    ("slack_api", "slack"),
    ("whatsapp_backup", "whatsapp"),
];

fn rename_type(t: &str) -> &str {
    RETIRED_TYPES
        .iter()
        .find(|(old, _)| *old == t)
        .map(|(_, new)| *new)
        .unwrap_or(t)
}

/// An ingest step's params, from the shape where the method was a `sync`
/// table (or implied by `common.input_path`) to one table per method
/// carrying its own `path`. `ty` is the group's type under its current
/// spelling. Params already in the current shape pass through untouched.
fn rewrite_ingest_params(ty: &str, params: &mut toml::Table) -> Result<()> {
    let input_path = params
        .get_mut("common")
        .and_then(|c| c.as_table_mut())
        .and_then(|c| c.remove("input_path"));
    if params
        .get("common")
        .and_then(|c| c.as_table())
        .is_some_and(|c| c.is_empty())
    {
        params.remove("common");
    }
    let mut sync = params.remove("sync");
    if sync.is_none() && input_path.is_none() {
        return Ok(());
    }
    let table_from_sync = |sync: Option<toml::Value>, name: &str| -> Result<toml::Table> {
        match sync {
            Some(toml::Value::Table(t)) => Ok(t),
            Some(other) => bail!("`sync` on a {ty} ingest step is {other}, not a table"),
            None => bail!(
                "a {ty} ingest step with no `sync` table names no method; add \
                 `[steps.params.{name}]` by hand"
            ),
        }
    };
    let with_path = |mut t: toml::Table, path: Option<toml::Value>| -> toml::Table {
        if let Some(p) = path {
            t.insert("path".into(), p);
        }
        t
    };
    let insert = |params: &mut toml::Table, name: &str, t: toml::Table| {
        params.insert(name.to_string(), toml::Value::Table(t));
    };
    match ty {
        "slack" | "chatgpt" | "github" | "gitlab" | "notion" | "yolink" => {
            insert(params, "api", table_from_sync(sync.take(), "api")?);
        }
        "claude" => {
            // `claude_api` had `sync`; `claude_export` had `input_path`.
            if let Some(p) = input_path {
                insert(params, "export", with_path(toml::Table::new(), Some(p)));
            } else {
                insert(params, "api", table_from_sync(sync.take(), "api")?);
            }
        }
        "email" => {
            if let Some(t) = sync.take() {
                params.insert("jmap".into(), t);
            }
            if let Some(p) = input_path {
                let mbox = match params.remove("mbox") {
                    Some(toml::Value::Table(t)) => t,
                    _ => toml::Table::new(),
                };
                insert(params, "mbox", with_path(mbox, Some(p)));
            }
        }
        "contacts" => {
            if let Some(t) = sync.take() {
                params.insert("carddav".into(), t);
            }
            if let Some(p) = input_path {
                insert(params, "vcf", with_path(toml::Table::new(), Some(p)));
            }
        }
        "beeper" => {
            let mut t = table_from_sync(sync.take(), "texts")?;
            if let Some(dir) = t.remove("beeper_data_dir") {
                t.insert("path".into(), dir);
            }
            insert(params, "texts", t);
        }
        "perseus" => {
            if input_path.is_some() {
                bail!(
                    "perseus: `common.input_path` on the ingest step has no home any more. \
                     A fetched tree is the ingest tree itself; a tree staged by hand is \
                     named on the *render* step's `common.input_path`, with no ingest step. \
                     Move or drop it, then run this again."
                );
            }
            insert(params, "github", table_from_sync(sync.take(), "github")?);
        }
        "signal" | "whatsapp" => {
            let mut t = table_from_sync(sync.take(), "backup")?;
            for old in ["snapshot_dir", "backup_dir"] {
                if let Some(dir) = t.remove(old) {
                    t.insert("path".into(), dir);
                }
            }
            insert(params, "backup", t);
        }
        "sms_backup_restore" => {
            insert(params, "backup", with_path(toml::Table::new(), input_path));
        }
        "google_takeout" => {
            let feeds = match sync.take() {
                Some(toml::Value::Table(t)) => t,
                _ => toml::Table::new(),
            };
            insert(params, "export", with_path(feeds, input_path));
        }
        "linkedin" => {
            insert(params, "export", with_path(toml::Table::new(), input_path));
        }
        "fsindex" | "pdf" | "media" => {
            insert(params, "fswalk", with_path(toml::Table::new(), input_path));
        }
        "lightroom" => {
            insert(params, "catalog", with_path(toml::Table::new(), input_path));
        }
        other => bail!(
            "no known method tables for type {other:?}; this tool rewrites only the \
             built-in providers' params"
        ),
    }
    if sync.is_some() {
        bail!("a {ty} ingest step has a `sync` table, and {ty} never selected its method that way");
    }
    Ok(())
}

/// What a retired-shape `datalib-step` step declared, read off its command
/// together with its id or group.
struct Builtin {
    group: String,
    /// The function under its current name.
    function: &'static str,
    r#type: Option<String>,
}

fn builtin_of(step: &StepIn) -> Option<Builtin> {
    let command = step.command.as_deref()?;
    let words: Vec<&str> = command.split_whitespace().collect();
    let prog = words.first()?;
    if !(*prog == "datalib-step" || prog.ends_with("/datalib-step")) {
        return None;
    }
    let group = match &step.group {
        Some(g) => g.clone(),
        None => {
            let id = step.id.as_deref()?;
            let (stem, leaf) = id.split_once('/')?;
            if leaf.contains('/') {
                return None;
            }
            stem.to_string()
        }
    };
    let typed = |function: &'static str| {
        Some(Builtin {
            group: group.clone(),
            function,
            r#type: Some(words.get(2)?.to_string()),
        })
    };
    let index = |function: &'static str| {
        Some(Builtin {
            group: group.clone(),
            function,
            r#type: None,
        })
    };
    match words.get(1).copied() {
        Some("download") => typed("ingest"),
        Some("render") => typed("render_markdown"),
        Some("grid_index") => index("grid_index"),
        Some("qmd_index") => index("qmd_index"),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
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

pub fn rewrite(text: &str) -> Result<String> {
    let cfg = parse_old(text)?;

    // Groups in the order they were declared, then in the order their
    // first step appears, so the output reads the way the input did.
    let mut groups: Vec<GroupOut> = cfg
        .groups
        .iter()
        .map(|g| GroupOut {
            id: g.id.clone(),
            name: g.name.clone(),
            r#type: g.r#type.as_deref().map(|t| rename_type(t).to_string()),
        })
        .collect();
    // Every built-in step's id moves with its function; everything that
    // named the old id — `inputs`, an applet's `tree` — follows it.
    let renames: BTreeMap<String, String> = cfg
        .steps
        .iter()
        .filter_map(|s| {
            let b = builtin_of(s)?;
            Some((s.old_id()?, format!("{}/{}", b.group, b.function)))
        })
        .collect();
    let rename = |id: &str| renames.get(id).cloned().unwrap_or_else(|| id.to_string());
    let rename_all = |ids: &[String]| ids.iter().map(|i| rename(i)).collect::<Vec<_>>();

    let mut steps: Vec<(Option<usize>, StepOut)> = Vec::with_capacity(cfg.steps.len());
    for step in &cfg.steps {
        let Some(b) = builtin_of(step) else {
            let gi = step
                .group
                .as_ref()
                .and_then(|g| groups.iter().position(|o| &o.id == g));
            let mut params = step.params.clone();
            // A command-less step is `datalib-step`'s, and its params are
            // the provider's to reshape; a custom step's are its own
            // program's.
            if step.command.is_none() {
                if let Some(toml::Value::Table(t)) = params.as_mut() {
                    strip_raw_path(t);
                    if step.function.as_deref() == Some("ingest") {
                        if let Some(ty) = gi.and_then(|gi| groups[gi].r#type.as_deref()) {
                            rewrite_ingest_params(ty, t).with_context(|| {
                                format!("step {}", step.old_id().unwrap_or_default())
                            })?;
                        }
                    }
                }
            }
            steps.push((
                gi,
                StepOut {
                    group: step.group.clone(),
                    function: step.function.clone(),
                    id: step.group.is_none().then(|| step.id.clone()).flatten(),
                    name: step.name.clone(),
                    command: step.command.clone(),
                    inputs: rename_all(&step.inputs),
                    env: step.env.clone(),
                    code_version: step.code_version.clone(),
                    params,
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
        let want = b.r#type.as_deref().map(|t| rename_type(t).to_string());
        match (&group.r#type, &want) {
            (Some(have), Some(want)) if have != want => bail!(
                "steps under {:?} disagree about its type: {have:?} and {want:?}",
                b.group
            ),
            (None, Some(want)) => group.r#type = Some(want.clone()),
            _ => {}
        }
        let mut params = step.params.clone();
        if let Some(toml::Value::Table(t)) = params.as_mut() {
            strip_raw_path(t);
            if b.function == "ingest" {
                if let Some(ty) = group.r#type.as_deref() {
                    rewrite_ingest_params(ty, t)
                        .with_context(|| format!("step {}", step.old_id().unwrap_or_default()))?;
                }
            }
        }
        // The download step's name is the source's name. The old wizard named
        // the render step `<that> (render markdown)`, falling back to the fetch
        // step's id, so a render step's name is only a fallback, and only once
        // that suffix and that fallback are removed. A step under a group
        // already has its name on the group.
        if step.group.is_none() {
            let old_id = step.old_id().unwrap_or_default();
            let name = step.name.as_deref().map(|n| {
                n.strip_suffix(" (render markdown)")
                    .unwrap_or(n)
                    .to_string()
            });
            let name = name.filter(|n| n != &old_id && n != &format!("{}/raw", b.group));
            if name.is_some() && (b.function == "ingest" || group.name.is_none()) {
                group.name = name;
            }
        }
        steps.push((
            Some(gi),
            StepOut {
                group: Some(b.group),
                function: Some(b.function.to_string()),
                id: None,
                name: None,
                command: None,
                inputs: rename_all(&step.inputs),
                env: step.env.clone(),
                code_version: step.code_version.clone(),
                params,
            },
        ));
    }

    let applets: Vec<AppletOut> = cfg
        .applets
        .iter()
        .map(|a| {
            let params = a.params.clone().map(|mut p| {
                if let Some(tree) = p.get("tree").and_then(|t| t.as_str()) {
                    let renamed = rename(tree);
                    if let Some(table) = p.as_table_mut() {
                        table.insert("tree".into(), toml::Value::String(renamed));
                    }
                }
                p
            });
            AppletOut {
                group: a.group.clone().or_else(|| applet_group(a, &groups)),
                id: a.id.clone(),
                command: a.command.clone(),
                env: a.env.clone(),
                params,
            }
        })
        .collect();

    let mut out = String::from(
        "# Rewritten by datalib-migrate-config into the current shape.\n\
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
    // A group nothing is filed under is still declared; keep it.
    for (gi, group) in groups.iter().enumerate() {
        if !emitted[gi] {
            out.push('\n');
            out.push_str(&block("groups", group)?);
        }
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
fn applet_group(a: &AppletIn, groups: &[GroupOut]) -> Option<String> {
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

fn header(cfg: &OldConfig) -> Result<String> {
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
        data_root: cfg.data_root.clone(),
        binary_dir: cfg.binary_dir.clone(),
        checkpoint_cadence: cfg.checkpoint_cadence,
    })
    .context("serialize the top-level keys")
}
