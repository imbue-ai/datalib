//! Turning a legacy `config.yaml` into a current `config.toml`.
//!
//! Two source formats, one destination:
//!
//! * [`steps_yaml_to_toml`] — a pre-TOML config already in the steps
//!   format. A straight reserialization: the steps and their params
//!   survive exactly, and YAML-only spellings are resolved on the way
//!   through (anchors in particular get expanded into the copies TOML
//!   needs).
//! * [`stanza_yaml_to_toml`] — the much older stanza-based `sources:`
//!   format ([`crate::legacy_stanza`]). A real schema translation: each
//!   source becomes a `<name>.download` + `<name>.render` step pair,
//!   with the legacy subtree split across the two phases.
//!
//! Both produce a reviewable draft, not a byte-faithful rewrite:
//! comments and formatting from the input are not carried over.
//!
//! Output is assembled as *text* — one serialized `[[steps]]` block per
//! step, glued together with comment dividers — rather than serializing
//! the whole config in one shot. Comments are the reason: a migrated
//! file wants section headers and commented-out disabled sources, and
//! neither survives a value-level serializer. Within a block we still
//! let `toml::to_string` do the work, so quoting, escaping, and the
//! values-before-tables ordering rule are never hand-rolled.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::Serialize;

use crate::legacy_stanza;

/// One step, shaped for output. Mirrors `datalib_dag::config::StepEntry`
/// but with `skip_serializing_if` throughout, so an absent field is
/// absent from the file rather than written as an empty list.
#[derive(Serialize)]
struct StepOut {
    id: String,
    command: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    /// Serialized last so its `[steps.params.…]` headers land after the
    /// plain keys — a table header ends the table it appears in.
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<toml::Value>,
}

/// The `unified_index` applet, as a `[[applets]]` block of text.
///
/// A converted config needs it for the same reason a scaffolded one
/// does: the grid, the document view and the document picker are served
/// by this applet, so a config without it opens an app with no search.
/// Emitted for both conversion paths.
fn unified_index_applet() -> String {
    "\n[[applets]]\nid = \"unified_index\"\ncommand = \"datalib-applet unified_index\"\n"
        .to_string()
}

/// One step as a `[[steps]]` block of text, ready to concatenate.
fn step_block(step: &StepOut) -> Result<String> {
    #[derive(Serialize)]
    struct One<'a> {
        steps: [&'a StepOut; 1],
    }
    toml::to_string(&One { steps: [step] }).with_context(|| format!("serialize step {:?}", step.id))
}

/// The top-level keys, which TOML requires above the first `[[steps]]`.
fn header(data_root: Option<PathBuf>, binary_dir: Option<PathBuf>) -> Result<String> {
    #[derive(Serialize)]
    struct Head {
        #[serde(skip_serializing_if = "Option::is_none")]
        data_root: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        binary_dir: Option<String>,
    }
    toml::to_string(&Head {
        data_root: data_root.map(|p| p.display().to_string()),
        binary_dir: binary_dir.map(|p| p.display().to_string()),
    })
    .context("serialize the top-level keys")
}

/// A full-width `# ── label ───────` section divider, padded to a fixed
/// width so the migrated file's sections are scannable.
fn divider(label: &str) -> String {
    const WIDTH: usize = 68;
    let pad = "\u{2500}".repeat(WIDTH.saturating_sub(label.chars().count()));
    format!("# \u{2500}\u{2500} {label} {pad}\n")
}

/// The pre-TOML steps schema, which this crate is now the only home
/// for.
///
/// It used to be read straight into the live `DagConfig`, since only
/// the parser differed. That stopped being true when a step's `id`
/// became the tree it writes: the live schema has no `outputs`, and
/// ids of the form `slack.download` are no longer valid. Both are
/// converted here — see [`upgraded_id`].
#[derive(Debug, serde::Deserialize)]
struct LegacyStepsConfig {
    #[serde(default)]
    data_root: Option<PathBuf>,
    #[serde(default)]
    binary_dir: Option<PathBuf>,
    #[serde(default)]
    steps: Vec<LegacyStep>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyStep {
    id: String,
    command: String,
    #[serde(default)]
    inputs: Vec<String>,
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    params: Option<toml::Value>,
}

/// A legacy step's id, in the current spelling: the tree it declared
/// writing. `slack.download` + `outputs: [slack/raw]` → `slack/raw`.
///
/// Falls back to the old id when the step declared no outputs, which
/// only happens for a malformed config — the runner then rejects it by
/// name, which is a better failure than inventing a path.
fn upgraded_id(step: &LegacyStep) -> String {
    step.outputs
        .first()
        .cloned()
        .unwrap_or_else(|| step.id.clone())
}

/// A legacy step's `inputs`, as step ids.
///
/// A legacy input already names an artifact path, and a step's id *is*
/// that path, so a concrete input converts to itself. A wildcard has no
/// counterpart in a world where inputs name steps — `**/rendered_md`
/// becomes the explicit list of render steps it used to match, which is
/// the same edge set written down.
fn upgraded_inputs(inputs: &[String], render_ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for i in inputs {
        if i.contains('*') {
            if i.ends_with("rendered_md") {
                out.extend(render_ids.iter().cloned());
            }
            // Any other wildcard matched nothing we can name; dropping
            // it is what the runner did with an unmatched wildcard too.
            continue;
        }
        out.push(i.clone());
    }
    out.sort();
    out.dedup();
    out
}

/// A pre-TOML steps config, re-emitted as TOML.
pub fn steps_yaml_to_toml(text: &str) -> Result<String> {
    // `params: Option<toml::Value>` deserializes fine from a YAML
    // document — the one thing that doesn't survive is YAML `null`,
    // which TOML cannot express and which errors with its key path.
    let cfg: LegacyStepsConfig =
        serde_yaml::from_str(text).context("parse the legacy config as YAML")?;

    // Old ids → new, so `inputs` can be rewritten from artifact paths
    // to the ids of the steps that write them. A legacy input already
    // names a path, and the new id *is* that path, so the mapping is
    // the identity for anything that resolved — but a wildcard input
    // (`**/rendered_md`) has no counterpart and must be expanded to the
    // steps it used to match.
    let render_ids: Vec<String> = cfg
        .steps
        .iter()
        .map(upgraded_id)
        .filter(|id| id.ends_with("/rendered_md"))
        .collect();

    let mut out = String::from(
        "# Converted from config.yaml. Comments and formatting from that\n\
         # file are not carried over; review before relying on it.\n\n",
    );
    out.push_str(&header(cfg.data_root, cfg.binary_dir)?);
    for e in &cfg.steps {
        out.push('\n');
        out.push_str(&step_block(&StepOut {
            id: upgraded_id(e),
            command: e.command.clone(),
            inputs: upgraded_inputs(&e.inputs, &render_ids),
            env: e.env.clone(),
            params: e.params.clone(),
        })?);
    }
    // A pre-TOML steps config predates applets entirely, so it never
    // named the one the app now needs. Add it rather than converting to
    // a config whose grid is empty.
    out.push('\n');
    out.push_str(&unified_index_applet());
    Ok(out)
}

/// The retired `sources:` config, translated to the step format: each
/// source becomes a `<name>.download` + `<name>.render` step pair
/// (render-only for unmanaged sources like `claude_export`), preceded
/// by the shared `grid_index`/`qmd_index` fan-in steps. Global
/// `defaults:` are folded into each source's `common:` (value-level
/// only — no path resolution), so the per-step params are
/// self-contained.
pub fn stanza_yaml_to_toml(text: &str) -> Result<String> {
    use std::fmt::Write as _;

    // Raw (un-normalized) parse: we want the fields as written, not
    // resolved absolute paths.
    let mut cfg: legacy_stanza::Config =
        serde_yaml::from_str(text).context("parse the legacy stanza config")?;
    let defaults = cfg.defaults.clone();

    let mut out = String::from(
        "# Migrated from the old sources: format. Review before relying\n\
         # on it; comments from the old file are not carried over.\n\n",
    );
    // Above the first [[steps]], as TOML requires.
    if !cfg.data_root.as_os_str().is_empty() {
        out.push_str(&header(Some(cfg.data_root.clone()), None)?);
    }

    // Every enabled source renders, so every enabled source feeds the
    // fan-ins. A disabled one is emitted commented out, so naming it
    // here would point at a step that isn't there.
    let render_ids: Vec<String> = cfg
        .sources
        .iter()
        .filter(|e| e.enabled)
        .map(|e| format!("{}/rendered_md", e.name))
        .collect();

    out.push('\n');
    out.push_str(&divider("shared fan-in steps"));
    out.push_str("# Every source's rendered markdown feeds these.\n");
    out.push_str(&step_block(&fanin(
        "unified_index/grid",
        "datalib-step grid_index",
        &render_ids,
    ))?);
    if !cfg.qmd.skip {
        out.push('\n');
        out.push_str(&step_block(&fanin(
            "unified_index/qmd",
            "datalib-step qmd_index",
            &render_ids,
        ))?);
    }

    out.push('\n');
    out.push_str(&divider("the app's own surface"));
    out.push_str("# Serves the grid; the app has no search without it.\n");
    out.push_str(&unified_index_applet());

    for entry in &mut cfg.sources {
        entry.source.common_mut().fold_defaults(&defaults);
        let name = entry.name.clone();
        let ty = entry.source.type_str();
        let managed = entry.source.is_managed();

        // The provider subtree, minus the `type:` tag (the command's
        // nested subcommand carries it now) and any nulls serde emitted
        // for unset fields. The old top-level `name:` is gone too — the
        // step derives it from its first declared output. Stripping the
        // nulls isn't cosmetic: TOML has no null, so a surviving one
        // would fail to serialize at all.
        let mut val = serde_yaml::to_value(&entry.source)
            .with_context(|| format!("serialize source {name:?}"))?;
        if let Some(m) = val.as_mapping_mut() {
            m.remove("type");
        }
        strip_nulls(&mut val);
        // Per-phase params split: pull the render-wave knobs out of the
        // legacy subtree into the render step's params; the rest is the
        // download step's. An empty side omits `params` entirely.
        let render_val = split_render_params(&mut val, ty, managed);

        let mut block = format!("\n{}", divider(&name));
        if managed {
            block.push_str(&step_block(&StepOut {
                id: format!("{name}/raw"),
                command: format!("datalib-step download {ty}"),
                inputs: Vec::new(),
                env: BTreeMap::new(),
                params: params_value(&val, &name)?,
            })?);
            block.push('\n');
        }
        block.push_str(&step_block(&StepOut {
            id: format!("{name}/rendered_md"),
            command: format!("datalib-step render {ty}"),
            // An unmanaged source has no download step to name, and
            // renders straight from the tree its `common.input_path`
            // points at — which is a param, not an artifact the DAG
            // knows about. So it declares no inputs.
            inputs: if managed {
                vec![format!("{name}/raw")]
            } else {
                Vec::new()
            },
            env: BTreeMap::new(),
            params: params_value(&render_val, &name)?,
        })?);

        if entry.enabled {
            out.push_str(&block);
        } else {
            // Disabled sources come over commented out — the step
            // format has no per-source enable flag.
            out.push_str("\n# (was `enabled: false` — uncomment to activate)\n");
            for line in block.trim_start_matches('\n').lines() {
                if line.is_empty() {
                    out.push_str("#\n");
                } else {
                    let _ = writeln!(out, "# {line}");
                }
            }
        }
    }
    Ok(out)
}

/// One of the two source-independent fan-in steps.
///
/// Its inputs are the render steps by id. The legacy format wrote
/// `**/rendered_md` here; a glob has no meaning once an input names a
/// step, so the same edge set is written down instead. That is also why
/// this takes the source list: the fan-ins are emitted before the
/// sources, but they can only be *written* once their names are known.
fn fanin(id: &str, command: &str, render_ids: &[String]) -> StepOut {
    StepOut {
        id: id.to_string(),
        command: command.to_string(),
        inputs: render_ids.to_vec(),
        env: BTreeMap::new(),
        params: None,
    }
}

/// A legacy params subtree as a TOML value, or `None` when there's
/// nothing in it.
///
/// Note an empty-but-present `sync` table is *not* nothing: its
/// presence is what makes a source managed, so [`strip_nulls`] keeps it
/// and this keeps the params block it lives in.
fn params_value(val: &serde_yaml::Value, name: &str) -> Result<Option<toml::Value>> {
    if val.as_mapping().is_none_or(|m| m.is_empty()) {
        return Ok(None);
    }
    toml::Value::try_from(val)
        .map(Some)
        .with_context(|| format!("source {name:?}: params → TOML"))
}

/// Pull the render-wave knobs out of a legacy source subtree (post
/// [`strip_nulls`]), returning the render step's params. These fields
/// moved off the shared stanza in the per-phase params split:
/// `sync.period` (beeper/signal) and `sync.alignment_pairs` (perseus)
/// hop out of `sync:` to the render params' top level;
/// `outlink_format` / `only_render_labels` (email) move over
/// verbatim. An explicit `common.raw_path` is *copied* (both phases
/// read the raw-store location); see below for `common.input_path`.
///
/// Only the keys `RenderCommon` accepts are copied over — the rest of
/// `SourceCommon` (`blob_size_limit_bytes`, `download_params`,
/// `event_tape`) is download-side and would be rejected by the render
/// config's `deny_unknown_fields`.
fn split_render_params(val: &mut serde_yaml::Value, ty: &str, managed: bool) -> serde_yaml::Value {
    use serde_yaml::{Mapping, Value};
    let mut render = Mapping::new();
    let Some(m) = val.as_mapping_mut() else {
        return Value::Mapping(render);
    };
    let worth_moving = |v: &Value| !v.as_sequence().is_some_and(|s| s.is_empty());
    if let Some(sync) = m
        .get_mut(Value::from("sync"))
        .and_then(|s| s.as_mapping_mut())
    {
        for key in ["period", "alignment_pairs"] {
            if let Some(v) = sync.remove(Value::from(key)) {
                if worth_moving(&v) {
                    render.insert(key.into(), v);
                }
            }
        }
    }
    for key in ["outlink_format", "only_render_labels"] {
        if let Some(v) = m.remove(Value::from(key)) {
            if worth_moving(&v) {
                render.insert(key.into(), v);
            }
        }
    }
    let mut rcommon = Mapping::new();
    if let Some(common) = m.get(Value::from("common")).and_then(|c| c.as_mapping()) {
        if let Some(v) = common.get(Value::from("raw_path")) {
            rcommon.insert("raw_path".into(), v.clone());
        }
        // `input_path` is normally download-side: the download step
        // consumes it and the render step reads the raw store. Two
        // cases render straight from the staged tree instead — perseus,
        // and any *unmanaged* source, which gets no download step at
        // all. Dropping it there would leave the render pointing at an
        // empty `<name>/raw` and silently produce nothing.
        if ty == "perseus" || !managed {
            if let Some(v) = common.get(Value::from("input_path")) {
                rcommon.insert("input_path".into(), v.clone());
            }
        }
    }
    if !rcommon.is_empty() {
        render.insert("common".into(), Value::Mapping(rcommon));
    }
    Value::Mapping(render)
}

/// Drop `key: null` entries and (bottom-up) mappings that emptied out —
/// serde emits both for unset optional/default fields, and TOML can't
/// express a null at all. `sync:` is kept even when empty: its
/// *presence* is what makes a source managed.
fn strip_nulls(v: &mut serde_yaml::Value) {
    if let Some(m) = v.as_mapping_mut() {
        for (_, val) in m.iter_mut() {
            strip_nulls(val);
        }
        let keys: Vec<serde_yaml::Value> = m
            .iter()
            .filter(|(k, val)| {
                val.is_null()
                    || (val.as_mapping().is_some_and(|mm| mm.is_empty())
                        && k.as_str() != Some("sync"))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            m.remove(&k);
        }
    }
}
