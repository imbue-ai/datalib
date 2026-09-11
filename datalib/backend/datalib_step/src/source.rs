//! What the runner tells a step about itself: the environment it sets and
//! the `--params` flag it appends.

use std::collections::BTreeMap;

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
pub const GROUP_DESCRIPTIONS_ENV: &str = "DATALIB_DAG_GROUP_DESCRIPTIONS";

/// The step as the runner declared it. `step` is the composed id, and
/// the one tree this process may write; the loader composed it from
/// `group` and `function`, and [`StepEnv::from_env`] checks that the
/// three still agree before anything is written.
#[derive(Debug, Clone)]
pub struct StepEnv {
    pub step: String,
    pub group: String,
    pub group_type: Option<String>,
    pub function: Function,
    /// The trees this step reads, data-root-relative, as the runner
    /// resolved them from the config's `inputs`.
    pub inputs: Vec<String>,
    /// The `description` of each group those trees are filed under, by
    /// group id — only the groups that wrote one. What the qmd index
    /// keeps as a collection's context.
    pub group_descriptions: BTreeMap<String, String>,
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
        let inputs = std::env::var(INPUTS_ENV)
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        let group_descriptions =
            parse_group_descriptions(std::env::var(GROUP_DESCRIPTIONS_ENV).ok().as_deref())?;
        Ok(StepEnv {
            step,
            group,
            group_type,
            function,
            inputs,
            group_descriptions,
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
    /// `ingest` tree, and says so.
    pub fn raw_store_rel(&self) -> String {
        match self.inputs.first() {
            Some(input) => input.clone(),
            None => {
                let rel = format!("{}/{}", self.group, Function::Ingest.as_str());
                tracing::warn!(
                    step = %self.step,
                    raw = %rel,
                    "render: this step declares no inputs; reading the group's own ingest tree"
                );
                rel
            }
        }
    }
}

fn parse_group_descriptions(raw: Option<&str>) -> Result<BTreeMap<String, String>> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(BTreeMap::new()),
        Some(s) => serde_json::from_str(s)
            .with_context(|| format!("{GROUP_DESCRIPTIONS_ENV} is not a JSON object of strings")),
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

pub fn parse_params(params: Option<&str>) -> Result<serde_json::Value> {
    match params {
        None => Ok(serde_json::Value::Object(Default::default())),
        Some(s) => {
            let v: serde_json::Value = serde_json::from_str(s)
                .context("parse --params as JSON (the provider's config subtree)")?;
            anyhow::ensure!(
                v.is_object(),
                "--params must be a JSON object (the provider's config subtree), got {v}"
            );
            Ok(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_verbatim_and_default_empty() {
        let p = parse_params(Some(r#"{"api":{"media":true}}"#)).unwrap();
        assert_eq!(p["api"]["media"], true);
        assert!(parse_params(None).unwrap().as_object().unwrap().is_empty());
        // A leftover old-format `type:` tag inside the params is passed
        // through; the provider config's deny_unknown/ignore rules
        // decide its fate downstream, not this layer.
        assert!(parse_params(Some(r#"{"type":"slack"}"#)).is_ok());
    }

    #[test]
    fn params_reject_non_objects_and_junk() {
        assert!(parse_params(Some("[1,2]")).is_err());
        assert!(parse_params(Some("not json")).is_err());
    }

    fn env(step: &str, group: &str, function: &str, inputs: &[&str]) -> StepEnv {
        StepEnv {
            step: step.into(),
            group: group.into(),
            group_type: Some("slack".into()),
            function: Function::parse(function).unwrap(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            group_descriptions: BTreeMap::new(),
        }
    }

    /// The runner leaves the variable unset when no group wrote a
    /// description; set, it is a JSON object and nothing else.
    #[test]
    fn group_descriptions_are_a_json_object_or_absent() {
        assert!(parse_group_descriptions(None).unwrap().is_empty());
        assert!(parse_group_descriptions(Some("  ")).unwrap().is_empty());
        let parsed =
            parse_group_descriptions(Some(r#"{"mail":"Fastmail, mostly receipts"}"#)).unwrap();
        assert_eq!(parsed["mail"], "Fastmail, mostly receipts");
        assert!(parse_group_descriptions(Some("[1]")).is_err());
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
    }
}
