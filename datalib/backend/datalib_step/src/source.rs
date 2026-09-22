//! What the runner tells a step about itself: the environment it sets and
//! the `--params-file` flag it appends.

use anyhow::{Context, Result};

use crate::function::Function;

/// Mirrors `datalib_dag::subprocess::ENV_*`; spelled out here because
/// `datalib-step` reads them as its contract with any runner, not as an
/// implementation detail of this one.
pub const STEP_ID_ENV: &str = "DATALIB_DAG_STEP";
pub const GROUP_ENV: &str = "DATALIB_DAG_GROUP";
pub const GROUP_TYPE_ENV: &str = "DATALIB_DAG_GROUP_TYPE";
pub const FUNCTION_ENV: &str = "DATALIB_DAG_FUNCTION";
pub const INPUTS_ENV: &str = "DATALIB_DAG_INPUTS";
/// Under a diff group only: the group whose raw store the diff compares,
/// and its `type`, which is the renderer this step runs.
pub const SOURCE_GROUP_ENV: &str = "DATALIB_DAG_SOURCE_GROUP";
pub const SOURCE_GROUP_TYPE_ENV: &str = "DATALIB_DAG_SOURCE_GROUP_TYPE";
/// The group `type` that means "render what changed in another group's
/// raw store" — `datalib_dag::config::DIFF_GROUP_TYPE`, spelled here as
/// this binary's side of the contract.
pub const DIFF_GROUP_TYPE: &str = "diff";

/// The step as the runner declared it. `step` is the composed id, and
/// the one tree this process may write; the loader composed it from
/// `group` and `function`, and [`StepEnv::from_env`] checks that the
/// three still agree before anything is written.
#[derive(Debug, Clone)]
pub struct StepEnv {
    pub step: String,
    pub group: String,
    pub group_type: Option<String>,
    /// Set only under a diff group: the source group and its type.
    pub source_group: Option<String>,
    pub source_group_type: Option<String>,
    pub function: Function,
    /// The trees this step reads, data-root-relative, as the runner
    /// resolved them from the config's `inputs`.
    pub inputs: Vec<String>,
}

impl StepEnv {
    pub fn from_env() -> Result<StepEnv> {
        let step = required(STEP_ID_ENV)?;
        let group = required(GROUP_ENV).context(
            "`datalib-step` runs only under a `[[groups]]` entry: it takes the provider \
             from the group's `type` and the tree it writes from `<group>/<function>`",
        )?;
        let function_word = required(FUNCTION_ENV)?;
        let function = Function::parse(&function_word).with_context(|| {
            format!(
                "`datalib-step` has no function {function_word:?}. It performs {}; a step \
                 doing anything else needs its own `command`.",
                Function::known_list()
            )
        })?;
        let composed = format!("{group}/{}", function.as_str());
        anyhow::ensure!(
            step == composed,
            "{STEP_ID_ENV}={step:?} but {GROUP_ENV}={group:?} and {FUNCTION_ENV}=\
             {function_word:?} compose to {composed:?}. A step writes only the tree its id \
             names, and the two must agree."
        );
        let group_type = std::env::var(GROUP_TYPE_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let source_group = std::env::var(SOURCE_GROUP_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let source_group_type = std::env::var(SOURCE_GROUP_TYPE_ENV)
            .ok()
            .filter(|t| !t.trim().is_empty());
        let inputs = std::env::var(INPUTS_ENV)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        Ok(StepEnv {
            step,
            group,
            group_type,
            source_group,
            source_group_type,
            function,
            inputs,
        })
    }

    pub fn is_diff_group(&self) -> bool {
        self.group_type.as_deref() == Some(DIFF_GROUP_TYPE)
    }

    /// The renderer a diff group's step runs: its source's type, which the
    /// loader put in this step's environment.
    pub fn diff_source_type(&self) -> Result<&str> {
        self.source_group_type.as_deref().with_context(|| {
            format!(
                "step {:?} is under diff group {:?} but {SOURCE_GROUP_TYPE_ENV} is not set; \
                 `datalib-dag` sets it from the group's `source`",
                self.step, self.group
            )
        })
    }

    /// The group's type, which is the provider to run. Only the two
    /// per-source functions need one; the index steps are typeless.
    pub fn source_type(&self) -> Result<&str> {
        self.group_type.as_deref().with_context(|| {
            format!(
                "step {:?} is `{}` under group {:?}, which declares no `type`; `datalib-step` \
                 needs the group's type to know which provider to run",
                self.step, self.function, self.group
            )
        })
    }

    /// The raw store a render reads: the first input, which is the
    /// ingest step's tree. A render with no inputs — a store seeded by
    /// hand, with no ingest step in front of it — reads the group's own
    /// `ingest` tree, and says so; under a diff group, its source's.
    pub fn raw_store_rel(&self) -> String {
        match self.inputs.first() {
            Some(input) => input.clone(),
            None => {
                let owner = self.source_group.as_deref().unwrap_or(&self.group);
                let rel = format!("{owner}/{}", Function::Ingest.as_str());
                tracing::warn!(
                    step = %self.step,
                    raw = %rel,
                    "this step declares no inputs; reading the group's own ingest tree"
                );
                rel
            }
        }
    }
}

fn required(name: &str) -> Result<String> {
    let v = std::env::var(name).with_context(|| {
        format!(
            "{name} is not set. `datalib-step` expects to be run by `datalib-dag`, which \
             sets it from the step's config entry."
        )
    })?;
    anyhow::ensure!(!v.trim().is_empty(), "{name} is set but empty");
    Ok(v)
}

/// The step's params: the JSON object in the file `--params-file` names,
/// or an empty one when the runner passed no file.
pub fn read_params(path: Option<&std::path::Path>) -> Result<serde_json::Value> {
    match path {
        None => Ok(serde_json::Value::Object(Default::default())),
        Some(p) => {
            let text = std::fs::read_to_string(p)
                .with_context(|| format!("read the params file {}", p.display()))?;
            parse_params(&text)
        }
    }
}

pub fn parse_params(text: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text)
        .context("parse the params file as JSON (the provider's config subtree)")?;
    anyhow::ensure!(
        v.is_object(),
        "the params file must hold a JSON object (the provider's config subtree), got {v}"
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
        assert_eq!(
            SOURCE_GROUP_TYPE_ENV,
            datalib_dag::subprocess::ENV_SOURCE_GROUP_TYPE
        );
        assert_eq!(SOURCE_GROUP_ENV, datalib_dag::subprocess::ENV_SOURCE_GROUP);
        assert_eq!(DIFF_GROUP_TYPE, datalib_dag::config::DIFF_GROUP_TYPE);
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

    fn env(step: &str, group: &str, function: &str, inputs: &[&str]) -> StepEnv {
        StepEnv {
            step: step.into(),
            group: group.into(),
            group_type: Some("slack".into()),
            source_group: None,
            source_group_type: None,
            function: Function::parse(function).unwrap(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// The raw store is whatever the render's input names — which is how
    /// a render can read an ingest under another group — and only falls
    /// back to the group's own tree when nothing is declared.
    #[test]
    fn render_reads_its_input_tree_else_the_groups_ingest() {
        let e = env(
            "slack/render_markdown",
            "slack",
            "render_markdown",
            &["elsewhere/ingest"],
        );
        assert_eq!(e.raw_store_rel(), "elsewhere/ingest");
        let e = env("slack/render_markdown", "slack", "render_markdown", &[]);
        assert_eq!(e.raw_store_rel(), "slack/ingest");
        let mut diff = env(
            "slack-diff/render_markdown",
            "slack-diff",
            "render_markdown",
            &[],
        );
        diff.group_type = Some(DIFF_GROUP_TYPE.into());
        diff.source_group = Some("slack".into());
        assert_eq!(diff.raw_store_rel(), "slack/ingest");
    }
}
