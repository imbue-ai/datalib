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
//! ```
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
//! want different knobs anyway (that's what the migrator's per-phase
//! split produces), so this is rarely the duplication it looks like.
//!
//! # Legacy YAML
//!
//! A config file named `.yaml`/`.yml` is parsed as YAML into the very
//! same structs, so data roots written before the TOML switch keep
//! working untouched. It's a read-only path — everything we *write*
//! is TOML — and the UI offers a one-click conversion. The one thing
//! that doesn't survive: YAML `null`, which TOML can't express, so a
//! legacy `params` containing one is rejected with a pointer to it.

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
}

/// Is this path a legacy YAML config? Decided purely by extension —
/// `.yaml` / `.yml` are YAML, everything else (including the canonical
/// `config.toml` and the extension-less paths the CLI accepts) is
/// TOML. Sniffing the contents instead would make a typo'd TOML file
/// silently reparse as YAML, which is a mapping of strings and would
/// then fail with a confusing schema error rather than a syntax one.
pub fn is_legacy_yaml_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml")
    )
}

/// Parse config text in the format implied by `path`'s extension. Both
/// formats target the identical structs, so the only difference is
/// which parser produces them — and which one's error messages you get.
pub fn parse(text: &str, path: &Path) -> Result<DagConfig> {
    if is_legacy_yaml_path(path) {
        serde_yaml::from_str(text).with_context(|| format!("parse {} as YAML", path.display()))
    } else {
        toml::from_str(text).with_context(|| format!("parse {}", path.display()))
    }
}

/// Load + resolve a config file. `data_root` defaults to the config
/// file's directory and gets `~` expanded.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg = parse(&text, path)?;
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

    /// Data roots written before the TOML switch still load, parsed as
    /// YAML purely on the strength of the `.yaml` extension.
    #[test]
    fn legacy_yaml_configs_still_load() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("config.yaml");
        std::fs::write(
            &p,
            "steps:\n  - id: slack.download\n    command: datalib-step download slack_api\n\
             \n    outputs: [slack/raw]\n    params: &slack\n      sync: {channels: [chat-qi]}\n\
             \n  - id: slack.render\n    command: datalib-step render slack_api\n\
             \n    inputs: [slack/raw]\n    outputs: [slack/rendered_md]\n    params: *slack\n",
        )
        .unwrap();
        let (cfg, root) = load(&p).unwrap();
        assert_eq!(root, std::fs::canonicalize(td.path()).unwrap());
        // Including YAML-only features like anchors, which the legacy
        // parser still honors even though TOML has no equivalent.
        assert_eq!(cfg.steps[0].params, cfg.steps[1].params);
        let specs = to_specs(&cfg).unwrap();
        assert_eq!(specs.len(), 2);

        // TOML can't express null, so a legacy config carrying one is
        // rejected — with the path to the offending key.
        let p = td.path().join("nulls.yaml");
        std::fs::write(
            &p,
            "steps:\n  - id: x\n    command: s\n    params: {k: null}\n",
        )
        .unwrap();
        let err = format!("{:#}", load(&p).unwrap_err());
        assert!(err.contains("params.k"), "{err}");
    }

    /// The extension picks the parser, so TOML syntax in a `.yaml`
    /// file (and vice versa) fails as a parse error naming the format
    /// we tried — never a silent reinterpretation.
    #[test]
    fn the_extension_picks_the_parser() {
        assert!(is_legacy_yaml_path(Path::new("/r/config.yaml")));
        assert!(is_legacy_yaml_path(Path::new("/r/config.yml")));
        assert!(!is_legacy_yaml_path(Path::new("/r/config.toml")));
        // The CLI takes any path; an unfamiliar extension means TOML.
        assert!(!is_legacy_yaml_path(Path::new("/r/pipeline")));

        let err = format!(
            "{:#}",
            parse("[[steps]]\nid = \"x\"\n", Path::new("c.yaml")).unwrap_err()
        );
        assert!(err.contains("as YAML"), "{err}");
    }
}
