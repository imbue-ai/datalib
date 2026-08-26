//! The DAG config file, `config.toml` — the format that replaced the
//! old stanza-based `sources:` config. The user declares the steps
//! directly; edges are still derived from artifact-path overlap, never
//! written by hand.
//!
//! ```toml
//! data_root = "~/datalib-data"     # default: the config file's dir
//! binary_dir = "/opt/datalib/bin"  # optional: prepended to PATH
//!
//! [[steps]]
//! id = "slack.download"
//! command = "datalib-step download slack_api"
//! outputs = ["slack/raw"]
//! # `params` is the provider's own config subtree. As a sub-table it
//! # must come after this step's plain keys — a TOML header ends the
//! # table it appears in.
//! [steps.params.sync]
//! channels = ["chat-qi"]
//!
//! [[steps]]
//! id = "grid_index"
//! command = "datalib-step grid_index"
//! inputs = ["**/rendered_md"]
//! outputs = ["system/backend_index"]
//!
//! [[steps]]
//! id = "custom"
//! command = "my-exporter --flag"   # any executable on PATH
//! outputs = ["custom/out"]
//!
//! [[applets]]
//! id = "slack_view"                # a JS identifier: it reaches card
//! title = "Slack"                  # source as a bare name
//! command = "datalib-applet slack"
//! [applets.params]
//! tree = "slack/rendered_md"
//! ```
//!
//! There are two kinds of entry. A **step** is scheduled: it reads and
//! writes artifacts, and the DAG is derived from those. An **applet**
//! is never scheduled — it is a long-lived server the http gateway
//! spawns on demand, contributing frontend components plus the
//! endpoints behind them, and it declares no inputs/outputs because it
//! owns no artifacts. See `docs/dev/applets.md`. This module parses and
//! validates both; only steps reach the scheduler.
//!
//! Note that top-level `data_root` / `binary_dir` must be written
//! *above* the first `[[steps]]`, since everything after a table
//! header belongs to that table.
//!
//! A step body is a `command` — a single string split shell-style
//! (quotes and backslash escapes, but no variable expansion or
//! globbing; wrap in `sh -c '…'` for real shell). The declared
//! `params` / `inputs` / `outputs` are appended to the argv as
//! `--params JSON` / `--inputs JSON` / `--outputs JSON`, each only
//! when present, so the command needs no TOML parser and the argv
//! stays reproducible. Any executable that understands those flags
//! (and optionally the NDJSON stdout protocol) can be a step — see
//! docs/dev/step_protocol.md.
//!
//! TOML has no anchors, so a params subtree shared between a download
//! and a render step is written out twice. In practice the two halves
//! want different knobs anyway, so this is rarely the duplication it
//! looks like.
//!
//! This is the *only* config format the runner accepts. Data roots
//! written before the TOML switch are converted once, out of band, by
//! the separate `datalib-migrate-config` program — which is where every
//! legacy schema and the last YAML parser live.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::step::{StepRun, StepSpec};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagConfig {
    /// Root for all artifacts. Optional: defaults to the directory the
    /// config file lives in, so a data root containing its own config
    /// is self-contained (same rule as the old format).
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    /// Directory prepended to `PATH` for every step subprocess, so
    /// commands can name binaries bare (`datalib-step …`). Optional;
    /// see [`resolve_binary_dir`] for the fallback chain.
    #[serde(default)]
    pub binary_dir: Option<PathBuf>,
    /// Defaults to empty: a config with no steps yet (a bare
    /// `data_root:` file) is valid — it just runs nothing.
    #[serde(default)]
    pub steps: Vec<StepEntry>,
    /// Long-lived servers that contribute the app's frontend and its
    /// data endpoints. Unlike steps these are never scheduled: the
    /// http gateway spawns one on demand when a request for its
    /// prefix arrives. Empty is normal — a data root with no applets
    /// still syncs and still serves the builtin UI.
    #[serde(default)]
    pub applets: Vec<AppletEntry>,
}

/// One applet instance. Deliberately a subset of [`StepEntry`]: an
/// applet declares no `inputs`/`outputs` because it is not scheduled
/// and owns no artifacts of its own — it reads what steps already
/// wrote. Everything else (`command` splitting, `params` as JSON,
/// `env` merge, cwd = data root) follows the step's conventions so
/// there is one set of rules to learn.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppletEntry {
    /// Instance name. Doubles as the mount prefix (`/applet/<id>/`) *and*
    /// as an identifier injected into card-source scope, so it is
    /// restricted to what JavaScript will accept as a variable name —
    /// see [`validate_applets`].
    pub id: String,
    /// Human label for the component gallery. Falls back to `id`.
    /// This is the field that makes two instances of one command
    /// distinguishable to a reader ("Work Slack" vs "Personal
    /// Slack"), so it earns its place in the schema.
    #[serde(default)]
    pub title: Option<String>,
    /// The command to run, split shell-style into an argv, resolved
    /// the same way a step's is (`binary_dir`, then `PATH`).
    pub command: String,
    /// Arbitrary applet parameters, forwarded verbatim as JSON via
    /// `--params` — to both the manifest dump and the server.
    #[serde(default)]
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl AppletEntry {
    /// The label to show a human. Config `title` when set, else the id.
    pub fn display_title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }

    /// `params` as JSON, ready for `--params`. `None` when the entry
    /// declared none.
    pub fn params_json(&self) -> Result<Option<serde_json::Value>> {
        match &self.params {
            Some(v) => Ok(Some(params_to_json(v, &self.id)?)),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepEntry {
    pub id: String,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// The command to run, split shell-style into an argv. Note the
    /// child runs with its cwd set to `data_root`, so a relative
    /// multi-component argv[0] resolves against the data root; use a
    /// bare name (PATH — see `binary_dir`) or an absolute path for
    /// binaries that live elsewhere.
    pub command: String,
    /// Arbitrary step parameters, forwarded verbatim as JSON via
    /// `--params`.
    #[serde(default)]
    pub params: Option<toml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional version of the step's own behavior, for steps whose
    /// output can change without their command line changing (a
    /// renderer that was reworked, a binary that was upgraded in
    /// place). Bumping it makes the runner re-run the step once, even
    /// though none of its inputs moved.
    #[serde(default)]
    pub code_version: Option<String>,
}

/// The config file inside a data root: `<data_root>/config.toml`. The
/// app reads and writes only this, which is what makes a data root
/// self-contained.
pub fn root_config_path(data_root: &Path) -> PathBuf {
    data_root.join(CONFIG_FILE_NAME)
}

/// The canonical config filename.
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// Parse config text. Errors carry TOML's own line/column, which is
/// what the UI surfaces in its editor.
pub fn parse(text: &str) -> Result<DagConfig> {
    toml::from_str(text).map_err(Into::into)
}

/// Load + resolve a config file. `data_root` defaults to the config
/// file's directory and gets `~` expanded.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg = parse(&text).with_context(|| format!("parse {}", path.display()))?;
    let data_root = match &cfg.data_root {
        Some(p) => expand_tilde(p),
        None => {
            let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            abs.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        }
    };
    Ok((cfg, data_root))
}

/// Turn config entries into scheduler specs: split each `command` and
/// append the declared `params`/`inputs`/`outputs` as `--flag JSON`
/// pairs (each only when present).
pub fn to_specs(cfg: &DagConfig) -> Result<Vec<StepSpec>> {
    let mut specs = Vec::with_capacity(cfg.steps.len());
    for e in &cfg.steps {
        let mut argv = shlex::split(&e.command).with_context(|| {
            format!(
                "step {:?}: command {:?} has unbalanced quoting",
                e.id, e.command
            )
        })?;
        if argv.is_empty() {
            bail!("step {:?}: empty command", e.id);
        }
        if let Some(params) = &e.params {
            let json = serde_json::to_string(&params_to_json(params, &e.id)?)
                .with_context(|| format!("step {:?}: params → JSON", e.id))?;
            argv.push("--params".to_string());
            argv.push(json);
        }
        if !e.inputs.is_empty() {
            argv.push("--inputs".to_string());
            argv.push(serde_json::to_string(&e.inputs).expect("string vec → JSON"));
        }
        if !e.outputs.is_empty() {
            argv.push("--outputs".to_string());
            argv.push(serde_json::to_string(&e.outputs).expect("string vec → JSON"));
        }
        let mut spec = StepSpec::new(
            &e.id,
            StepRun::Subprocess {
                argv,
                env: e.env.clone(),
            },
        );
        spec.code_version = e.code_version.clone();
        for i in &e.inputs {
            spec.inputs
                .push(crate::ArtifactPat::parse(i).with_context(|| format!("step {:?}", e.id))?);
        }
        for o in &e.outputs {
            spec.outputs
                .push(crate::ArtifactPat::parse(o).with_context(|| format!("step {:?}", e.id))?);
        }
        specs.push(spec);
    }
    Ok(specs)
}

/// A step's `params` subtree as the JSON the child gets on `--params`.
///
/// Serializing `toml::Value` straight through serde would be wrong:
/// TOML's date/time types have no JSON counterpart, and the `toml`
/// crate smuggles them past a non-TOML serializer as a one-key map
/// (`{"$__toml_private_datetime": …}`), which is what the step would
/// then try to deserialize. So walk the tree and render every datetime
/// as its RFC-3339-ish string — `since = 2026-06-15` and
/// `since = "2026-06-15"` reach the step identically, which is what a
/// user writing either one expects.
fn params_to_json(v: &toml::Value, step: &str) -> Result<serde_json::Value> {
    use serde_json::Value as J;
    Ok(match v {
        toml::Value::String(s) => J::String(s.clone()),
        toml::Value::Integer(i) => J::from(*i),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => J::Number(n),
            // TOML has nan/inf literals; JSON has no way to say them,
            // so refuse here rather than hand the child a null it
            // would read as "unset".
            None => bail!(
                "step {step:?}: params has a non-finite float ({f}), which JSON can't represent"
            ),
        },
        toml::Value::Array(a) => J::Array(
            a.iter()
                .map(|x| params_to_json(x, step))
                .collect::<Result<_>>()?,
        ),
        toml::Value::Table(t) => J::Object(
            t.iter()
                .map(|(k, x)| Ok((k.clone(), params_to_json(x, step)?)))
                .collect::<Result<_>>()?,
        ),
    })
}

/// Locate the directory prepended to every step's `PATH`. Precedence:
/// CLI override (`--binary-dir`), then config `binary_dir:`, then the
/// running executable's own directory — a packaged release lays the
/// step binaries next to the runner. `None` only when even the
/// executable path is unknowable; steps then get the inherited `PATH`
/// untouched.
///
/// Relative paths are absolutized against the *runner's* cwd, because
/// steps are spawned with their cwd set to `data_root` — a relative
/// `--binary-dir bazel-bin/...` would otherwise be re-resolved against
/// the data root.
pub fn resolve_binary_dir(cfg: &DagConfig, cli_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = cli_override {
        return Some(absolutize(expand_tilde(p)));
    }
    if let Some(p) = &cfg.binary_dir {
        return Some(absolutize(expand_tilde(p)));
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p,
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// The one namespace an applet may not claim.
///
/// Frontend components all live under `system/frontend/<namespace>/`,
/// and a refresh *deletes* every namespace directory before asking the
/// applets to rewrite theirs. `user` holds hand- and agent-authored
/// components, which nothing regenerates — so an applet allowed to take
/// that id would have its directory wiped on the next refresh, taking
/// the user's own work with it.
pub const RESERVED_APPLET_ID: &str = "user";

/// Check the applet list before anything tries to use it.
///
/// Three rules, all load-bearing rather than stylistic:
///
///   * **Ids are JavaScript identifiers.** An applet id is injected
///     into card-source scope as a bare name (`slack_work.channels()`),
///     and card source is evaluated by `new Function`, so an id like
///     `slack.work` or `2fa` would be a syntax error at the point a
///     card renders — far from the config that caused it. Reject it
///     here, where the message can name the file.
///   * **Ids are unique.** They are the proxy prefix and the namespace;
///     two entries claiming one id would make `/applet/<id>/` ambiguous.
///     TOML cannot enforce this for us since `[[applets]]` is an array.
///   * **`user` is reserved.** See [`RESERVED_APPLET_ID`].
pub fn validate_applets(cfg: &DagConfig) -> Result<()> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for a in &cfg.applets {
        if !is_js_identifier(&a.id) {
            bail!(
                "applet {:?}: id must be a JavaScript identifier (letters, digits, _ or $, \
                 not starting with a digit) because it is injected into card source as a \
                 bare name",
                a.id
            );
        }
        if a.id == RESERVED_APPLET_ID {
            bail!(
                "applet id {RESERVED_APPLET_ID:?} is reserved: it names the namespace for \
                 components the user (or an agent) authors, which the app owns and never \
                 overwrites. Pick another id."
            );
        }
        if seen.insert(a.id.as_str(), ()).is_some() {
            bail!("applet {:?}: duplicate id", a.id);
        }
        if a.command.trim().is_empty() {
            bail!("applet {:?}: empty command", a.id);
        }
    }
    Ok(())
}

/// Conservative subset of what JavaScript accepts: ASCII only. The
/// language would allow plenty of Unicode, but an id is also a URL
/// path segment and a directory-safe token elsewhere, so the narrow
/// rule is the useful one.
fn is_js_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => return false,
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        Some(_) => return false,
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$') {
        return false;
    }
    // Reserved words would parse as syntax, not as a binding.
    !matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "let"
            | "static"
            | "yield"
            | "await"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_gets_declared_fields_as_json_flags() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "slack.download"
            outputs = ["slack/raw"]
            command = "datalib-step download slack_api"
            params.sync = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            id = "slack.render"
            inputs = ["slack/raw"]
            outputs = ["slack/rendered_md"]
            command = "datalib-step render slack_api"
            params.sync = {media = true, channels = ["chat-qi"], since = "2026-06-15"}

            [[steps]]
            id = "grid_index"
            inputs = ["**/rendered_md"]
            outputs = ["system/backend_index"]
            command = "datalib-step grid_index"
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        assert_eq!(specs.len(), 3);

        let argv = |i: usize| match &specs[i].run {
            StepRun::Subprocess { argv, .. } => argv.clone(),
            other => panic!("expected subprocess, got {other:?}"),
        };
        let dl = argv(0);
        assert_eq!(&dl[..3], &["datalib-step", "download", "slack_api"]);
        assert_eq!(dl[3], "--params");
        let params: serde_json::Value = serde_json::from_str(&dl[4]).unwrap();
        assert_eq!(params["sync"]["channels"][0], "chat-qi");
        // No inputs declared → no --inputs; outputs follow params.
        assert_eq!(&dl[5..], &["--outputs", r#"["slack/raw"]"#]);

        // TOML has no anchors, so the render step repeats the subtree —
        // and must produce byte-identical JSON for it.
        let rn = argv(1);
        assert_eq!(rn[1], "render");
        assert_eq!(rn[4], dl[4]);
        assert_eq!(
            &rn[5..],
            &[
                "--inputs",
                r#"["slack/raw"]"#,
                "--outputs",
                r#"["slack/rendered_md"]"#
            ]
        );

        // Param-less step: just inputs + outputs.
        assert_eq!(
            argv(2),
            vec![
                "datalib-step",
                "grid_index",
                "--inputs",
                r#"["**/rendered_md"]"#,
                "--outputs",
                r#"["system/backend_index"]"#
            ]
        );

        // The graph derives as expected from the declared artifacts.
        let g = crate::Graph::build(specs).unwrap();
        assert_eq!(g.deps[g.by_id["grid_index"]].len(), 1);
    }

    #[test]
    fn command_splits_shell_style() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "custom"
            outputs = ["custom/out"]
            command = """sh -c 'echo "hi there" > custom/out/x.txt'"""
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        match &specs[0].run {
            StepRun::Subprocess { argv, .. } => {
                assert_eq!(
                    &argv[..3],
                    &["sh", "-c", r#"echo "hi there" > custom/out/x.txt"#]
                );
                assert_eq!(&argv[3..], &["--outputs", r#"["custom/out"]"#]);
            }
            other => panic!("expected subprocess, got {other:?}"),
        }
    }

    #[test]
    fn bad_commands_are_rejected() {
        let cfg: DagConfig = toml::from_str(
            r#"steps = [{id = "x", outputs = ["x/raw"], command = "unbalanced '"}]"#,
        )
        .unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("unbalanced quoting"), "{err}");

        let cfg: DagConfig =
            toml::from_str(r#"steps = [{id = "x", outputs = ["x/raw"], command = ""}]"#).unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("empty command"), "{err}");
    }

    #[test]
    fn missing_command_is_rejected_at_parse() {
        let err = toml::from_str::<DagConfig>(r#"steps = [{id = "x", outputs = ["x/out"]}]"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("command"), "{err}");
    }

    /// TOML dates are a distinct scalar type with no JSON counterpart;
    /// they must reach the step as the string the user typed, not as
    /// the `toml` crate's internal `$__toml_private_datetime` wrapper.
    #[test]
    fn toml_datetimes_reach_the_step_as_strings() {
        let cfg: DagConfig = toml::from_str(
            r#"
            [[steps]]
            id = "x"
            outputs = ["x/raw"]
            command = "s"
            params.sync = {since = 2026-06-15, at = 2026-06-15T10:30:00Z}
            "#,
        )
        .unwrap();
        let specs = to_specs(&cfg).unwrap();
        let StepRun::Subprocess { argv, .. } = &specs[0].run else {
            panic!("expected subprocess");
        };
        let params: serde_json::Value = serde_json::from_str(&argv[2]).unwrap();
        assert_eq!(params["sync"]["since"], "2026-06-15");
        assert_eq!(params["sync"]["at"], "2026-06-15T10:30:00Z");
    }

    #[test]
    fn binary_dir_resolution_prefers_cli_then_config() {
        let cfg: DagConfig = toml::from_str(r#"binary_dir = "/opt/datalib/bin""#).unwrap();
        assert_eq!(
            resolve_binary_dir(&cfg, None),
            Some(PathBuf::from("/opt/datalib/bin"))
        );
        // CLI override wins, and relative paths are pinned to the
        // runner's cwd (children run with cwd = data_root).
        let got = resolve_binary_dir(&cfg, Some(Path::new("bazel-bin/x"))).unwrap();
        assert_eq!(got, std::env::current_dir().unwrap().join("bazel-bin/x"));

        // No CLI/config → the runner executable's own directory.
        let cfg: DagConfig = toml::from_str("").unwrap();
        let got = resolve_binary_dir(&cfg, None).unwrap();
        assert_eq!(
            got,
            std::env::current_exe().unwrap().parent().unwrap(),
            "default is the running executable's directory"
        );
    }

    #[test]
    fn data_root_defaults_to_config_dir() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("pipeline.toml");
        std::fs::write(&p, "steps = []\n").unwrap();
        let (_cfg, root) = load(&p).unwrap();
        assert_eq!(root, std::fs::canonicalize(td.path()).unwrap());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<DagConfig>("step_bin = \"/x\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}

#[cfg(test)]
mod applet_tests {
    use super::*;

    fn cfg(text: &str) -> DagConfig {
        parse(text).expect("parse")
    }

    #[test]
    fn applets_default_to_empty() {
        let c = cfg("data_root = \"/tmp/x\"\n");
        assert!(c.applets.is_empty());
        validate_applets(&c).expect("empty list is valid");
    }

    #[test]
    fn parses_an_applet_with_params_and_title() {
        let c = cfg(r#"
[[applets]]
id = "slack_work"
title = "Work Slack"
command = "datalib-applet slack"
[applets.params]
tree = "slack_work/rendered_md"
"#);
        assert_eq!(c.applets.len(), 1);
        let a = &c.applets[0];
        assert_eq!(a.id, "slack_work");
        assert_eq!(a.display_title(), "Work Slack");
        let params = a.params_json().unwrap().expect("params present");
        assert_eq!(params["tree"], "slack_work/rendered_md");
    }

    #[test]
    fn title_falls_back_to_id() {
        let c = cfg("[[applets]]\nid = \"grid\"\ncommand = \"x\"\n");
        assert_eq!(c.applets[0].display_title(), "grid");
        assert!(c.applets[0].params_json().unwrap().is_none());
    }

    /// The id reaches card source as a bare identifier, so a dotted or
    /// digit-leading id would blow up inside `new Function` at render
    /// time rather than at config load.
    #[test]
    fn rejects_ids_that_are_not_js_identifiers() {
        for bad in ["slack.work", "2fa", "has-dash", "", "with space", "class"] {
            let c = cfg(&format!("[[applets]]\nid = \"{bad}\"\ncommand = \"x\"\n"));
            assert!(
                validate_applets(&c).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn accepts_ordinary_identifiers() {
        for good in ["grid", "slack_work", "_priv", "$x", "a1"] {
            let c = cfg(&format!("[[applets]]\nid = \"{good}\"\ncommand = \"x\"\n"));
            validate_applets(&c).unwrap_or_else(|e| panic!("{good:?} rejected: {e}"));
        }
    }

    /// `user` is where hand-authored components live and a refresh
    /// wipes every applet namespace, so letting an applet claim it
    /// would delete the user's own work.
    #[test]
    fn rejects_the_reserved_user_id() {
        let c = cfg("[[applets]]\nid = \"user\"\ncommand = \"x\"\n");
        let err = validate_applets(&c).expect_err("\"user\" must be refused");
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn rejects_duplicate_ids() {
        let c = cfg(r#"
[[applets]]
id = "slack"
command = "a"

[[applets]]
id = "slack"
command = "b"
"#);
        let err = validate_applets(&c).expect_err("duplicate must fail");
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    /// Two instances of one command is the case the whole design is
    /// built around; it must parse without complaint.
    #[test]
    fn two_instances_of_one_command_are_fine() {
        let c = cfg(r#"
[[applets]]
id = "a"
command = "datalib-applet slack"
[applets.params]
tree = "a/rendered_md"

[[applets]]
id = "b"
command = "datalib-applet slack"
[applets.params]
tree = "b/rendered_md"
"#);
        validate_applets(&c).expect("distinct ids, same command");
        assert_eq!(c.applets.len(), 2);
    }
}
