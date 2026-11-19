//! `datalib-step check-params`: plan every datalib-step step's params in
//! a config the way a sync would, and run nothing. The runner links no
//! provider and cannot read params itself, so the app asks this when a
//! config is saved; the example configs' tests hold to the same check.

use std::path::Path;

use datalib_dag::config::DagConfig;

use crate::dispatch::{self, Phase};
use crate::function::Function;
use crate::source::StepEnv;

/// One datalib-step step whose params a sync would refuse. A line of
/// `check-params`' output, as JSON.
#[derive(Debug)]
pub struct Problem {
    pub step: String,
    pub message: String,
}

pub fn problems(cfg: &DagConfig, data_root: &Path) -> Vec<Problem> {
    cfg.steps
        .iter()
        .filter_map(|step| {
            let args = crate::step_entry::run_args(&step.command)?;
            plan_step(step, args, data_root)
                .err()
                .map(|message| Problem {
                    step: step.id.clone(),
                    message,
                })
        })
        .collect()
}

fn plan_step(
    step: &datalib_dag::config::StepEntry,
    args: Result<crate::source::RunArgs, String>,
    data_root: &Path,
) -> Result<(), String> {
    let args = args.map_err(|e| format!("its command does not parse: {e}"))?;
    let env =
        StepEnv::new(step.id.clone(), &args, step.inputs.clone()).map_err(|e| format!("{e:#}"))?;
    let mut params = match &step.params {
        Some(p) => serde_json::to_value(p).map_err(|e| format!("params → JSON: {e}"))?,
        None => serde_json::json!({}),
    };
    let phase = match env.function {
        Function::RenderOccurrences => {
            return crate::occurrences::params(env.group_type.as_deref(), params)
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
        }
        Function::Ingest => Phase::Ingest,
        Function::RenderMarkdown => Phase::Render,
        _ => return Ok(()),
    };
    // A diff group plans as its source's renderer, once `params.diff` is
    // taken off — the same split a run makes.
    let ty = if env.is_diff_group() {
        let (_pair, rest) =
            crate::render_diff::split_params(params).map_err(|e| format!("{e:#}"))?;
        params = rest;
        env.diff_source_type().map_err(|e| format!("{e:#}"))?
    } else {
        env.source_type().map_err(|e| format!("{e:#}"))?
    };
    let raw_dir = datalib_etl::layout::ingest_root(data_root, &env.group);
    dispatch::plan(ty, phase, &env.group, raw_dir, params)
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

    /// The case `check-params` exists for: a config that loads, whose
    /// params a sync would refuse only once the step ran.
    #[test]
    fn a_misspelled_param_is_named_against_its_step() {
        let found = check(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\nid = \"slack/ingest\"\n\
             command = \"datalib-step run ingest slack\"\n\n[steps.params.api]\n\
             chanels = [\"bridge\"]\n",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].step, "slack/ingest");
        assert!(found[0].message.contains("chanels"), "{found:?}");
    }

    #[test]
    fn good_params_and_custom_commands_pass() {
        let found = check(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\nid = \"slack/ingest\"\n\
             command = \"datalib-step run ingest slack\"\n\n[steps.params.api]\n\
             channels = [\"bridge\"]\n\n[[steps]]\nid = \"mine/thing\"\ncommand = \"true\"\n",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// A datalib-step step whose command does not say what to do, or
    /// whose id is not `<group>/<function>`, is named, not skipped.
    #[test]
    fn a_step_that_would_not_start_is_named() {
        let found = check(
            "[[steps]]\nid = \"slack/ingest\"\ncommand = \"datalib-step run\"\n\n\
             [[steps]]\nid = \"slack/fetch\"\ncommand = \"datalib-step run ingest slack\"\n\n\
             [[steps]]\nid = \"mail/ingest\"\ncommand = \"datalib-step run ingest\"\n",
        );
        let named: Vec<&str> = found.iter().map(|p| p.step.as_str()).collect();
        assert_eq!(
            named,
            ["slack/ingest", "slack/fetch", "mail/ingest"],
            "{found:?}"
        );
    }
}
