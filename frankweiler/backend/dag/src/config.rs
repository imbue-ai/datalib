//! The DAG config file — the new-format replacement for the old
//! stanza-based `config.yaml`. The user declares the steps directly;
//! edges are still derived from artifact-path overlap, never written
//! by hand.
//!
//! ```yaml
//! data_root: ~/frankweiler-data     # default: the config file's dir
//! binary_dir: /opt/frankweiler/bin  # optional: prepended to PATH
//! steps:
//!   - id: slack.download
//!     command: datalib-step download slack_api
//!     outputs: [slack/raw]
//!     params:                       # the provider's own config subtree
//!       sync: {channels: [chat-qi]}
//!   - id: grid_index
//!     command: datalib-step grid_index
//!     inputs: ["**/rendered_md"]
//!     outputs: [system/backend_index]
//!   - id: custom
//!     command: my-exporter --flag   # any executable on PATH
//!     outputs: [custom/out]
//! ```
//!
//! A step body is a `command:` — a single string split shell-style
//! (quotes and backslash escapes, but no variable expansion or
//! globbing; wrap in `sh -c '…'` for real shell). The declared
//! `params` / `inputs` / `outputs` are appended to the argv as
//! `--params JSON` / `--inputs JSON` / `--outputs JSON`, each only
//! when present, so the command needs no YAML parser and the argv
//! stays reproducible. Any executable that understands those flags
//! (and optionally the NDJSON stdout protocol) can be a step — see
//! docs/dev/step_protocol.md. YAML anchors (`&slack` / `*slack`)
//! remain handy for sharing one params subtree between a download and
//! render step pair.

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
    pub params: Option<serde_yaml::Value>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Load + resolve a config file. `data_root` defaults to the config
/// file's directory and gets `~` expanded.
pub fn load(path: &Path) -> Result<(DagConfig, PathBuf)> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: DagConfig =
        serde_yaml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
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
            // YAML → canonical JSON. serde_yaml::Value serializes
            // directly; non-string map keys (which JSON can't express)
            // error out here rather than in the child.
            let json = serde_json::to_string(params)
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
        let cfg: DagConfig = serde_yaml::from_str(
            r#"
            steps:
              - id: slack.download
                outputs: [slack/raw]
                command: datalib-step download slack_api
                params: &slack
                  sync: {media: true, channels: [chat-qi], since: "2026-06-15"}
              - id: slack.render
                inputs: [slack/raw]
                outputs: [slack/rendered_md]
                command: datalib-step render slack_api
                params: *slack
              - id: grid_index
                inputs: ["**/rendered_md"]
                outputs: [system/backend_index]
                command: datalib-step grid_index
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

        // The YAML anchor shares the same params with the render step.
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
        let cfg: DagConfig = serde_yaml::from_str(
            r#"
            steps:
              - id: custom
                outputs: [custom/out]
                command: sh -c 'echo "hi there" > custom/out/x.txt'
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
        let cfg: DagConfig =
            serde_yaml::from_str(r#"steps: [{id: x, outputs: [x/raw], command: "unbalanced '"}]"#)
                .unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("unbalanced quoting"), "{err}");

        let cfg: DagConfig =
            serde_yaml::from_str(r#"steps: [{id: x, outputs: [x/raw], command: ""}]"#).unwrap();
        let err = to_specs(&cfg).unwrap_err().to_string();
        assert!(err.contains("empty command"), "{err}");
    }

    #[test]
    fn missing_command_is_rejected_at_parse() {
        let err = serde_yaml::from_str::<DagConfig>("steps: [{id: x, outputs: [x/out]}]")
            .unwrap_err()
            .to_string();
        assert!(err.contains("command"), "{err}");
    }

    #[test]
    fn binary_dir_resolution_prefers_cli_then_config() {
        let cfg: DagConfig =
            serde_yaml::from_str("steps: []\nbinary_dir: /opt/frankweiler/bin").unwrap();
        assert_eq!(
            resolve_binary_dir(&cfg, None),
            Some(PathBuf::from("/opt/frankweiler/bin"))
        );
        // CLI override wins, and relative paths are pinned to the
        // runner's cwd (children run with cwd = data_root).
        let got = resolve_binary_dir(&cfg, Some(Path::new("bazel-bin/x"))).unwrap();
        assert_eq!(got, std::env::current_dir().unwrap().join("bazel-bin/x"));

        // No CLI/config → the runner executable's own directory.
        let cfg: DagConfig = serde_yaml::from_str("steps: []").unwrap();
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
        let p = td.path().join("pipeline.yaml");
        std::fs::write(&p, "steps: []\n").unwrap();
        let (_cfg, root) = load(&p).unwrap();
        assert_eq!(root, std::fs::canonicalize(td.path()).unwrap());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_yaml::from_str::<DagConfig>("steps: []\nstep_bin: /x\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}
