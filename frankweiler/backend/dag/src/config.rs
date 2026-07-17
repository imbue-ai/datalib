//! The DAG config file — the new-format replacement for the old
//! stanza-based `config.yaml`. The user declares the steps directly;
//! edges are still derived from artifact-path overlap, never written
//! by hand.
//!
//! ```yaml
//! data_root: ~/frankweiler-data     # default: the config file's dir
//! steps:
//!   - id: slack.download
//!     step: slack_api.download      # <source_type>.<phase>
//!     outputs: [slack/raw]
//!     params:
//!       name: slack                 # → slack/raw, slack/rendered_md
//!       source:                     # the provider's own config subtree
//!         sync: {channels: [chat-qi]}
//!   - id: index
//!     step: index                   # source-independent step types stay bare
//!     inputs: ["**/rendered_md"]
//!     outputs: [system/backend_index]
//!   - id: custom
//!     outputs: [custom/out]
//!     run: [sh, -c, "…"]            # or: any argv, verbatim
//! ```
//!
//! A step body is either `run:` (verbatim argv) or `step:` + optional
//! `params:` — sugar for invoking the `datalib-step` binary, which
//! hosts the real step-type implementations. Download and render are
//! per-provider step types written `<source_type>.<phase>` (the step
//! type names the provider, so params carry no `type:` tag); `index`
//! and `qmd` are genuinely source-independent and stay bare. Params
//! are passed as canonical JSON via `--params-json` so the child
//! needs no YAML parser and the argv stays a single line. YAML
//! anchors (`&slack` / `*slack`) remain handy for sharing one source
//! definition between a download and render step pair.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::step::{StepRun, StepSpec};

/// Basename of the step-type host binary the `step:` sugar invokes.
pub const STEP_BIN_NAME: &str = "datalib-step";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DagConfig {
    /// Root for all artifacts. Optional: defaults to the directory the
    /// config file lives in, so a data root containing its own config
    /// is self-contained (same rule as the old format).
    #[serde(default)]
    pub data_root: Option<PathBuf>,
    /// Explicit path to the `datalib-step` binary. Optional; see
    /// [`resolve_step_bin`] for the fallback chain.
    #[serde(default)]
    pub step_bin: Option<PathBuf>,
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
    /// Verbatim argv. Mutually exclusive with `step`. Note the child
    /// runs with its cwd set to `data_root`, so a relative
    /// multi-component argv[0] resolves against the data root; use a
    /// bare name (PATH) or an absolute path for binaries that live
    /// elsewhere.
    #[serde(default)]
    pub run: Option<Vec<String>>,
    /// A `datalib-step` step type: `<source_type>.download` /
    /// `<source_type>.render` (e.g. `slack_api.download`), or a bare
    /// source-independent type (`index`, `qmd`). Mutually exclusive
    /// with `run`.
    #[serde(default)]
    pub step: Option<String>,
    /// Parameters for `step:`, forwarded as JSON via `--params-json`.
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

/// Run-wide options threaded onto every `step:`-typed entry's argv.
/// Raw `run:` entries are never touched — these encode the
/// `datalib-step` CLI, which arbitrary argv doesn't speak.
#[derive(Debug, Clone, Default)]
pub struct StepTypeOpts {
    /// Pinned run timestamp, appended as `--now <v>` to every step
    /// type (a `datalib-step` global flag) so all stamped outputs
    /// agree — required for deterministic runs.
    pub now: Option<String>,
    /// Appended as `--reset-and-redownload` to `download` steps only.
    pub reset_and_redownload: bool,
    /// Appended as `--refetch-blobs` to `download` steps only.
    pub refetch_blobs: bool,
}

/// Turn config entries into scheduler specs, expanding the `step:`
/// sugar against `step_bin`.
pub fn to_specs(cfg: &DagConfig, step_bin: &Path, opts: &StepTypeOpts) -> Result<Vec<StepSpec>> {
    let mut specs = Vec::with_capacity(cfg.steps.len());
    for e in &cfg.steps {
        let argv = match (&e.run, &e.step) {
            (Some(_), Some(_)) => {
                bail!("step {:?}: `run` and `step` are mutually exclusive", e.id)
            }
            (None, None) => bail!("step {:?}: needs either `run` or `step`", e.id),
            (Some(argv), None) => {
                if e.params.is_some() {
                    bail!("step {:?}: `params` only applies to `step:` entries", e.id);
                }
                if argv.is_empty() {
                    bail!("step {:?}: empty argv", e.id);
                }
                argv.clone()
            }
            (None, Some(step_type)) => {
                // `<source_type>.<phase>` (e.g. `slack_api.download`)
                // maps to `datalib-step <phase> --type <source_type>`;
                // a bare token (`index`, `qmd`) is a source-independent
                // step type invoked as a plain subcommand.
                let mut argv = vec![step_bin.to_string_lossy().into_owned()];
                let phase = match step_type.rsplit_once('.') {
                    Some((source_type, phase @ ("download" | "render"))) => {
                        argv.push(phase.to_string());
                        argv.push("--type".to_string());
                        argv.push(source_type.to_string());
                        phase
                    }
                    Some((_, other)) => bail!(
                        "step {:?}: unknown phase {other:?} in step type {step_type:?} \
                         (expected <source_type>.download or <source_type>.render)",
                        e.id
                    ),
                    None if step_type == "download" || step_type == "render" => bail!(
                        "step {:?}: `{step_type}` needs a source type — write \
                         `<source_type>.{step_type}` (e.g. `slack_api.{step_type}`)",
                        e.id
                    ),
                    None => {
                        argv.push(step_type.clone());
                        step_type.as_str()
                    }
                };
                if let Some(params) = &e.params {
                    // YAML → canonical JSON. serde_yaml::Value
                    // serializes directly; non-string map keys (which
                    // JSON can't express) error out here rather than
                    // in the child.
                    let json = serde_json::to_string(params)
                        .with_context(|| format!("step {:?}: params → JSON", e.id))?;
                    argv.push("--params-json".to_string());
                    argv.push(json);
                }
                if let Some(now) = &opts.now {
                    argv.push("--now".to_string());
                    argv.push(now.clone());
                }
                if phase == "download" {
                    if opts.reset_and_redownload {
                        argv.push("--reset-and-redownload".to_string());
                    }
                    if opts.refetch_blobs {
                        argv.push("--refetch-blobs".to_string());
                    }
                }
                argv
            }
        };
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

/// Locate the `datalib-step` binary. Precedence: CLI override, then
/// config `step_bin:`, then a sibling of the running executable, then
/// the bare name (PATH lookup at spawn time).
///
/// Relative paths are absolutized against the *runner's* cwd here,
/// because steps are spawned with their cwd set to `data_root` — a
/// relative `--step-bin bazel-bin/...` would otherwise be re-resolved
/// against the data root and fail to spawn. A bare name (single
/// component) is left alone so it stays a PATH lookup.
pub fn resolve_step_bin(cfg: &DagConfig, cli_override: Option<&Path>) -> PathBuf {
    if let Some(p) = cli_override {
        return absolutize(p.to_path_buf());
    }
    if let Some(p) = &cfg.step_bin {
        return absolutize(expand_tilde(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(STEP_BIN_NAME);
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from(STEP_BIN_NAME)
}

fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() || p.components().count() == 1 {
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
    fn step_sugar_expands_to_datalib_step_argv() {
        let cfg: DagConfig = serde_yaml::from_str(
            r#"
            steps:
              - id: slack.download
                outputs: [slack/raw]
                step: slack_api.download
                params: &slack
                  name: slack
                  source:
                    sync: {media: true, channels: [chat-qi], since: "2026-06-15"}
              - id: slack.render
                inputs: [slack/raw]
                outputs: [slack/rendered_md]
                step: slack_api.render
                params: *slack
              - id: index
                inputs: ["**/rendered_md"]
                outputs: [system/backend_index]
                step: index
            "#,
        )
        .unwrap();
        let specs = to_specs(
            &cfg,
            Path::new("/opt/bin/datalib-step"),
            &StepTypeOpts::default(),
        )
        .unwrap();
        assert_eq!(specs.len(), 3);

        let argv = |i: usize| match &specs[i].run {
            StepRun::Subprocess { argv, .. } => argv.clone(),
            other => panic!("expected subprocess, got {other:?}"),
        };
        let dl = argv(0);
        assert_eq!(
            &dl[..4],
            &["/opt/bin/datalib-step", "download", "--type", "slack_api"]
        );
        assert_eq!(dl[4], "--params-json");
        let params: serde_json::Value = serde_json::from_str(&dl[5]).unwrap();
        assert_eq!(params["name"], "slack");
        // No type tag inside the params — the step type carries it.
        assert!(params["source"].get("type").is_none());
        assert_eq!(params["source"]["sync"]["channels"][0], "chat-qi");

        // The YAML anchor shares the same params with the render step.
        let rn = argv(1);
        assert_eq!(&rn[1..4], &["render", "--type", "slack_api"]);
        assert_eq!(rn[5], dl[5]);

        // Param-less source-independent step type stays bare.
        assert_eq!(argv(2), vec!["/opt/bin/datalib-step", "index"]);

        // The graph derives as expected from the declared artifacts.
        let g = crate::Graph::build(specs).unwrap();
        assert_eq!(g.deps[g.by_id["index"]].len(), 1);
    }

    #[test]
    fn step_type_opts_thread_now_and_reset_flags() {
        let cfg: DagConfig = serde_yaml::from_str(
            r#"
            steps:
              - id: slack.download
                outputs: [slack/raw]
                step: slack_api.download
                params: {name: slack, source: {sync: {}}}
              - id: index
                inputs: [slack/raw]
                outputs: [system/backend_index]
                step: index
              - id: custom
                outputs: [custom/out]
                run: [echo, hi]
            "#,
        )
        .unwrap();
        let opts = StepTypeOpts {
            now: Some("2026-07-17T00:00:00-07:00".into()),
            reset_and_redownload: true,
            refetch_blobs: false,
        };
        let specs = to_specs(&cfg, Path::new("datalib-step"), &opts).unwrap();
        let argv = |i: usize| match &specs[i].run {
            StepRun::Subprocess { argv, .. } => argv.clone(),
            other => panic!("expected subprocess, got {other:?}"),
        };
        // download gets --now AND the reset flag.
        let dl = argv(0);
        assert!(dl
            .windows(2)
            .any(|w| w == ["--now", "2026-07-17T00:00:00-07:00"]));
        assert!(dl.iter().any(|a| a == "--reset-and-redownload"));
        assert!(!dl.iter().any(|a| a == "--refetch-blobs"));
        // index gets --now but never the download-only flags.
        let idx = argv(1);
        assert!(idx
            .windows(2)
            .any(|w| w == ["--now", "2026-07-17T00:00:00-07:00"]));
        assert!(!idx.iter().any(|a| a == "--reset-and-redownload"));
        // raw argv entries are untouched.
        assert_eq!(argv(2), vec!["echo", "hi"]);
    }

    #[test]
    fn bare_download_and_bad_phase_are_rejected() {
        let cfg: DagConfig =
            serde_yaml::from_str("steps: [{id: x, outputs: [x/raw], step: download}]").unwrap();
        let err = to_specs(&cfg, Path::new("datalib-step"), &StepTypeOpts::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs a source type"), "{err}");

        let cfg: DagConfig =
            serde_yaml::from_str("steps: [{id: x, outputs: [x/raw], step: slack_api.reticulate}]")
                .unwrap();
        let err = to_specs(&cfg, Path::new("datalib-step"), &StepTypeOpts::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown phase"), "{err}");
    }

    #[test]
    fn run_and_step_are_mutually_exclusive() {
        let cfg: DagConfig =
            serde_yaml::from_str("steps: [{id: x, outputs: [x/out], run: [echo], step: index}]")
                .unwrap();
        let err = to_specs(&cfg, Path::new("datalib-step"), &StepTypeOpts::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn params_require_step() {
        let cfg: DagConfig =
            serde_yaml::from_str("steps: [{id: x, outputs: [x/out], run: [echo], params: {a: 1}}]")
                .unwrap();
        let err = to_specs(&cfg, Path::new("datalib-step"), &StepTypeOpts::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("only applies"), "{err}");
    }

    #[test]
    fn relative_step_bin_is_absolutized_against_runner_cwd() {
        // Steps spawn with cwd = data_root, so a relative
        // `--step-bin bazel-bin/...` must be pinned to the runner's
        // cwd up front or the spawn fails with ENOENT.
        let cfg: DagConfig = serde_yaml::from_str("steps: []").unwrap();
        let got = resolve_step_bin(&cfg, Some(Path::new("bazel-bin/x/datalib_step")));
        assert!(got.is_absolute(), "{got:?}");
        assert_eq!(
            got,
            std::env::current_dir()
                .unwrap()
                .join("bazel-bin/x/datalib_step")
        );

        // Bare name stays bare (PATH lookup at spawn time).
        let bare = resolve_step_bin(&cfg, Some(Path::new("datalib-step")));
        assert_eq!(bare, PathBuf::from("datalib-step"));

        // Absolute paths pass through untouched, incl. from `step_bin:`.
        let cfg2: DagConfig =
            serde_yaml::from_str("steps: []\nstep_bin: /opt/bin/datalib-step").unwrap();
        assert_eq!(
            resolve_step_bin(&cfg2, None),
            PathBuf::from("/opt/bin/datalib-step")
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
        let err = serde_yaml::from_str::<DagConfig>("steps: []\nsources: []\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }
}
