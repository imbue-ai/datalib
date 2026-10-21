//! `datalib-step check-params`: plan every built-in step's params in a
//! config the way a sync would, and run nothing. `datalib-dag --check`
//! asks it, since the runner links no provider and cannot read params
//! itself; the example configs' tests hold to the same check.

use std::path::Path;

use datalib_dag::config::{DagConfig, DIFF_GROUP_TYPE};

use crate::dispatch::{self, Phase};
use crate::function::Function;

/// One built-in step whose params a sync would refuse. A line of
/// `check-params`' output, as JSON.
#[derive(Debug)]
pub struct Problem {
    pub step: String,
    pub message: String,
}

pub fn problems(cfg: &DagConfig, data_root: &Path) -> Vec<Problem> {
    cfg.steps
        .iter()
        .filter(|s| s.command.is_none())
        .filter_map(|step| {
            plan_step(cfg, step, data_root)
                .err()
                .map(|message| Problem {
                    step: step.id.clone(),
                    message,
                })
        })
        .collect()
}

fn plan_step(
    cfg: &DagConfig,
    step: &datalib_dag::config::StepEntry,
    data_root: &Path,
) -> Result<(), String> {
    let (Some(group), Some(function)) = (step.group.as_deref(), step.function.as_deref()) else {
        return Err("has no command and no group".to_string());
    };
    let phase = match Function::parse(function) {
        Some(Function::Ingest) => Phase::Ingest,
        Some(Function::RenderMarkdown) => Phase::Render,
        Some(_) => return Ok(()),
        None => return Err(format!("datalib-step has no function {function:?}")),
    };
    let group_entry = cfg
        .groups
        .iter()
        .find(|g| g.id == group)
        .ok_or_else(|| format!("its group {group:?} is not declared"))?;
    let mut params = match &step.params {
        Some(p) => serde_json::to_value(p).map_err(|e| format!("params → JSON: {e}"))?,
        None => serde_json::json!({}),
    };
    // A diff group plans as its source's renderer, once `params.diff` is
    // taken off — the same split a run makes.
    let ty = if group_entry.r#type.as_deref() == Some(DIFF_GROUP_TYPE) {
        let (_pair, rest) =
            crate::render_diff::split_params(params).map_err(|e| format!("{e:#}"))?;
        params = rest;
        let source = group_entry
            .source
            .as_deref()
            .ok_or_else(|| format!("diff group {group:?} names no source"))?;
        cfg.groups
            .iter()
            .find(|g| g.id == source)
            .and_then(|g| g.r#type.as_deref())
            .ok_or_else(|| format!("diff group {group:?}: its source {source:?} has no type"))?
    } else {
        group_entry
            .r#type
            .as_deref()
            .ok_or_else(|| format!("its group {group:?} declares no type"))?
    };
    let raw_dir = datalib_etl::layout::ingest_root(data_root, group);
    dispatch::plan(ty, phase, group, raw_dir, params)
        .map(|_| ())
        .map_err(|e| format!("{e:#}"))
}

/// Prints one JSON [`Problem`] per line and exits 0 when the config could
/// be read, whatever it found; 1 when it could not.
#[allow(clippy::disallowed_macros)]
pub fn run_cli(path: &Path) -> i32 {
    let (checked, data_root) = match datalib_dag::config::load_graded(path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    if checked.is_fatal() {
        eprintln!("{} is not a config", path.display());
        return 1;
    }
    for p in problems(&checked.cfg, &data_root) {
        println!(
            "{}",
            serde_json::json!({"step": p.step, "message": p.message})
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Vec<Problem> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let (checked, root) = datalib_dag::config::load_graded(&path).unwrap();
        problems(&checked.cfg, &root)
    }

    /// The case `--check` exists for: a config that loads, whose params a
    /// sync would refuse only once the step ran.
    #[test]
    fn a_misspelled_param_is_named_against_its_step() {
        let found = check(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\ngroup = \"slack\"\n\
             function = \"ingest\"\n\n[steps.params.api]\nchanels = [\"bridge\"]\n",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].step, "slack/ingest");
        assert!(found[0].message.contains("chanels"), "{found:?}");
    }

    #[test]
    fn good_params_and_custom_commands_pass() {
        let found = check(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\ngroup = \"slack\"\n\
             function = \"ingest\"\n\n[steps.params.api]\nchannels = [\"bridge\"]\n\n\
             [[steps]]\nid = \"mine/thing\"\ncommand = \"true\"\n",
        );
        assert!(found.is_empty(), "{found:?}");
    }
}
