//! What a step knows about itself: its id from the runner's environment,
//! and its function and its group's type from its own argv, `datalib-step
//! run <function> [<type>] [--source-type <type>]`, which its config entry's
//! `command` spells.

use anyhow::{Context, Result};

use crate::function::Function;

pub const STEP_ID_ENV: &str = datalib_env::DATALIB_DAG_STEP;
pub const INPUTS_ENV: &str = datalib_env::DATALIB_DAG_INPUTS;
pub const PARAMS_ENV: &str = datalib_env::DATALIB_DAG_PARAMS;
/// The group `type` that means "render what changed in another group's
/// raw store": the source's own type then comes in `--source-type`.
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The words after `run` in a step's command.
#[derive(clap::Args, Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// What the step does: `ingest`, `render_markdown`, `grid_index`, …
    /// The second part of its id, which is `<group>/<function>`.
    pub function: String,
    /// Its group's `type`: the provider (`slack`, `email`, …), or `diff`.
    /// Left out under an index group, which has none.
    #[arg(value_name = "TYPE")]
    pub group_type: Option<String>,
    /// Under a `diff` group only: the type of the source whose raw store
    /// the diff renders.
    #[arg(long, value_name = "TYPE")]
    pub source_type: Option<String>,
}

/// The step as its config entry declares it. `step` is the id, and the
/// one tree this process may write.
#[derive(Debug, Clone)]
pub struct StepEnv {
    pub step: String,
    pub group: String,
    pub group_type: Option<String>,
    /// Set only under a diff group: the type of the source it renders.
    pub diff_source_type: Option<String>,
    pub function: Function,
    /// The trees this step reads, data-root-relative, as the runner
    /// resolved them from the config's `inputs`.
    pub inputs: Vec<String>,
}

impl StepEnv {
    /// From `DATALIB_DAG_STEP`, `DATALIB_DAG_INPUTS` and the argv.
    pub fn from_env(args: &RunArgs) -> Result<StepEnv> {
        let step = std::env::var(STEP_ID_ENV).with_context(|| {
            format!(
                "{STEP_ID_ENV} is not set. `datalib-step run` expects to be run by \
                 `datalib-dag`, which sets it to the step's id."
            )
        })?;
        let inputs = std::env::var(INPUTS_ENV)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        StepEnv::new(step, args, inputs)
    }

    pub fn new(step: String, args: &RunArgs, inputs: Vec<String>) -> Result<StepEnv> {
        let function = Function::parse(&args.function).with_context(|| {
            format!(
                "`datalib-step` has no function {:?}. It performs {}; a step doing anything \
                 else needs its own `command`.",
                args.function,
                Function::known_list()
            )
        })?;
        let group = match step.split_once('/') {
            Some((group, rest)) if !group.is_empty() && rest == function.as_str() => {
                group.to_string()
            }
            _ => anyhow::bail!(
                "this step's id is {step:?}, but a step `datalib-step run {function}` runs must \
                 be `<group>/{function}`: the step writes only the tree its id names, and \
                 `datalib-step` finds the trees it reads by that name"
            ),
        };
        let group_type = args.group_type.clone().filter(|t| !t.trim().is_empty());
        let diff_source_type = args.source_type.clone().filter(|t| !t.trim().is_empty());
        let is_diff = group_type.as_deref() == Some(DIFF_GROUP_TYPE);
        anyhow::ensure!(
            is_diff || diff_source_type.is_none(),
            "`--source-type` is for a step under a `{DIFF_GROUP_TYPE}` group, and {step:?} \
             is not one"
        );
        Ok(StepEnv {
            step,
            group,
            group_type,
            diff_source_type,
            function,
            inputs,
        })
    }

    pub fn is_diff_group(&self) -> bool {
        self.group_type.as_deref() == Some(DIFF_GROUP_TYPE)
    }

    /// The renderer a diff group's step runs: its source's type.
    pub fn diff_source_type(&self) -> Result<&str> {
        self.diff_source_type.as_deref().with_context(|| {
            format!(
                "step {:?} is under a `{DIFF_GROUP_TYPE}` group, so its command names the \
                 source's type: `datalib-step run {} {DIFF_GROUP_TYPE} --source-type <type>`",
                self.step, self.function
            )
        })
    }

    /// The group's type, which is the provider to run. Only the per-source
    /// functions need one; the index steps are typeless.
    pub fn source_type(&self) -> Result<&str> {
        self.group_type.as_deref().with_context(|| {
            format!(
                "step {:?} is `{}`, which needs its group's type to know which provider to \
                 run: `datalib-step run {} <type>`",
                self.step, self.function, self.function
            )
        })
    }

    /// The raw store a render reads: the first input, which is the
    /// ingest step's tree. A render with no inputs — a store seeded by
    /// hand, with no ingest step in front of it — reads the group's own
    /// `ingest` tree, and says so. A diff has no tree of its own to fall
    /// back on.
    pub fn raw_store_rel(&self) -> Result<String> {
        if let Some(input) = self.inputs.first() {
            return Ok(input.clone());
        }
        anyhow::ensure!(
            !self.is_diff_group(),
            "step {:?} is under a `{DIFF_GROUP_TYPE}` group and reads nothing: its `inputs` \
             name the source's `<group>/ingest`",
            self.step
        );
        let rel = format!("{}/{}", self.group, Function::Ingest.as_str());
        tracing::warn!(
            step = %self.step,
            raw = %rel,
            "this step declares no inputs; reading the group's own ingest tree"
        );
        Ok(rel)
    }
}

/// The step's params: the JSON object in `DATALIB_DAG_PARAMS`, or an
/// empty one when the step has none.
pub fn params_from_env() -> Result<serde_json::Value> {
    match std::env::var(PARAMS_ENV) {
        Ok(text) if !text.trim().is_empty() => {
            parse_params(&text).with_context(|| format!("read {PARAMS_ENV}"))
        }
        _ => Ok(empty_params()),
    }
}

/// The params in the file a utility's `--params-file` names, or an empty
/// object when it names none.
pub fn read_params(path: Option<&std::path::Path>) -> Result<serde_json::Value> {
    match path {
        None => Ok(empty_params()),
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("read the params file {}", p.display()))?;
            parse_params(&text)
        }
    }
}

fn empty_params() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

pub fn parse_params(text: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text)
        .context("parse the params as JSON (the provider's config subtree)")?;
    anyhow::ensure!(
        v.is_object(),
        "the params must be a JSON object (the provider's config subtree), got {v}"
    );
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_verbatim_and_default_empty() {
        let p = parse_params(r#"{"api":{"media":true}}"#).unwrap();
        assert_eq!(p["api"]["media"], true);
        assert!(read_params(None).unwrap().as_object().unwrap().is_empty());
        // A leftover old-format `type:` tag inside the params is passed
        // through; the provider config's deny_unknown/ignore rules
        // decide its fate downstream, not this layer.
        assert!(parse_params(r#"{"type":"slack"}"#).is_ok());
    }

    #[test]
    fn the_env_names_agree_with_the_runners() {
        assert_eq!(PARAMS_ENV, datalib_dag::subprocess::ENV_PARAMS);
        assert_eq!(STEP_ID_ENV, datalib_dag::subprocess::ENV_STEP);
        assert_eq!(INPUTS_ENV, datalib_dag::subprocess::ENV_INPUTS);
    }

    #[test]
    fn params_reject_non_objects_and_junk() {
        assert!(parse_params("[1,2]").is_err());
        assert!(parse_params("not json").is_err());
    }

    #[test]
    fn params_come_from_the_named_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("params.json");
        std::fs::write(&path, r#"{"api":{"channels":["c"]}}"#).unwrap();
        let p = read_params(Some(&path)).unwrap();
        assert_eq!(p["api"]["channels"][0], "c");
        let err = read_params(Some(&dir.path().join("missing.json")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("read the params file"), "{err}");
    }

    fn args(function: &str, group_type: Option<&str>, source_type: Option<&str>) -> RunArgs {
        RunArgs {
            function: function.into(),
            group_type: group_type.map(Into::into),
            source_type: source_type.map(Into::into),
        }
    }

    /// The id is `<group>/<function>`, and the function is the one the
    /// command names: a step writes only its own tree, and datalib-step
    /// finds every tree by that name.
    #[test]
    fn the_step_id_must_be_the_group_and_the_function() {
        let ok = StepEnv::new(
            "work-slack/ingest".into(),
            &args("ingest", Some("slack"), None),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(ok.group, "work-slack");
        assert_eq!(ok.source_type().unwrap(), "slack");
        for bad in [
            "work-slack/render_markdown",
            "ingest",
            "/ingest",
            "a/b/ingest",
        ] {
            let err = StepEnv::new(bad.into(), &args("ingest", Some("slack"), None), Vec::new())
                .unwrap_err()
                .to_string();
            assert!(err.contains("<group>/ingest"), "{bad}: {err}");
        }
        let err = StepEnv::new("x/fetch".into(), &args("fetch", None, None), Vec::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no function \"fetch\""), "{err}");
    }

    #[test]
    fn a_source_type_is_only_for_a_diff_group() {
        let diff = StepEnv::new(
            "slack-diff/render_markdown".into(),
            &args("render_markdown", Some("diff"), Some("slack")),
            vec!["slack/ingest".into()],
        )
        .unwrap();
        assert!(diff.is_diff_group());
        assert_eq!(diff.diff_source_type().unwrap(), "slack");
        assert!(StepEnv::new(
            "slack/render_markdown".into(),
            &args("render_markdown", Some("slack"), Some("slack")),
            Vec::new(),
        )
        .is_err());
        let no_source = StepEnv::new(
            "slack-diff/render_markdown".into(),
            &args("render_markdown", Some("diff"), None),
            Vec::new(),
        )
        .unwrap();
        assert!(no_source.diff_source_type().is_err());
    }

    /// The raw store is whatever the render's input names — which is how
    /// a render can read an ingest under another group — and only falls
    /// back to the group's own tree when nothing is declared.
    #[test]
    fn render_reads_its_input_tree_else_the_groups_ingest() {
        let env = |step: &str, ty: &str, source: Option<&str>, inputs: &[&str]| {
            StepEnv::new(
                step.into(),
                &args("render_markdown", Some(ty), source),
                inputs.iter().map(|s| s.to_string()).collect(),
            )
            .unwrap()
        };
        let e = env(
            "slack/render_markdown",
            "slack",
            None,
            &["elsewhere/ingest"],
        );
        assert_eq!(e.raw_store_rel().unwrap(), "elsewhere/ingest");
        let e = env("slack/render_markdown", "slack", None, &[]);
        assert_eq!(e.raw_store_rel().unwrap(), "slack/ingest");
        let diff = env("slack-diff/render_markdown", "diff", Some("slack"), &[]);
        assert!(diff.raw_store_rel().is_err());
    }
}
