//! Plan a step's params the way a sync would, and run nothing: what
//! `run … --check` does (`step_protocol.md` § Check), which the app sends on
//! a save and `datalib-pipeline check` sends from a terminal; and the same
//! for every datalib-step step of a file, which `examples --append` and
//! the example configs' tests hold to.

use std::path::Path;

use datalib_config::DagConfig;

use crate::dispatch::{self, Phase};
use crate::function::Function;
use crate::source::StepEnv;

/// One datalib-step step whose params a sync would refuse.
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
    step: &datalib_config::StepEntry,
    args: Result<crate::source::RunArgs, String>,
    data_root: &Path,
) -> Result<(), String> {
    let args = args.map_err(|e| format!("its command does not parse: {e}"))?;
    let env =
        StepEnv::new(step.id.clone(), &args, step.inputs.clone()).map_err(|e| format!("{e:#}"))?;
    let params = match &step.params {
        Some(p) => serde_json::to_value(p).map_err(|e| format!("params → JSON: {e}"))?,
        None => serde_json::Value::Null,
    };
    check(&env, params, data_root).map(|_warnings| ())
}

/// Whether this step would run `params` as written: the warnings it would
/// run with, such as a param that does nothing, or why it would refuse.
/// `Null` params are a step that wrote none.
pub fn check(
    env: &StepEnv,
    params: serde_json::Value,
    data_root: &Path,
) -> Result<Vec<String>, String> {
    let wrote_none = params.is_null();
    let mut params = if wrote_none {
        serde_json::json!({})
    } else {
        params
    };
    let phase = match env.function {
        Function::RenderOccurrences => {
            return crate::occurrences::params(env.group_type.as_deref(), params)
                .map(|_| Vec::new())
                .map_err(|e| format!("{e:#}"));
        }
        Function::Embed => {
            let params = if wrote_none {
                serde_json::Value::Null
            } else {
                params
            };
            return crate::modal::params(params)
                .map(|_| Vec::new())
                .map_err(|e| format!("{e:#}"));
        }
        Function::Ingest => Phase::Ingest,
        Function::RenderMarkdown => Phase::Render,
        _ => return Ok(Vec::new()),
    };
    let warnings: Vec<String> = crate::methods::drop_inert_params(&mut params.clone())
        .into_iter()
        .map(|path| format!("`{path}` no longer does anything; delete it"))
        .collect();
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
        .map(|_| warnings)
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Vec<Problem> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, text).unwrap();
        let (checked, root) = datalib_dag::config_check::load_graded(&path).unwrap();
        problems(&checked.cfg, &root)
    }

    /// The case the check exists for: a config that loads, whose params
    /// a sync would refuse only once the step ran.
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

    /// An `embed` step's one parameter is planned like any other: a value
    /// that is not a whole number, or a name that is not it, is named
    /// against its step.
    #[test]
    fn an_embed_step_s_threshold_is_planned() {
        let embed = |params: &str| {
            check(&format!(
                "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\nid = \"slack/embed\"\n\
                 command = \"datalib-step run embed slack\"\n{params}"
            ))
        };
        assert!(embed("").is_empty());
        assert!(embed("[steps.params]\nembed_on_modal_from_n_chunks = 5000\n").is_empty());
        for bad in [
            "[steps.params]\nembed_on_modal_from_n_chunks = \"lots\"\n",
            "[steps.params]\nembed_on_modal_from_n_chunks = -1\n",
            "[steps.params]\nembed_on_modal = 5000\n",
        ] {
            let found = embed(bad);
            assert_eq!(found.len(), 1, "{bad}: {found:?}");
            assert_eq!(found[0].step, "slack/embed");
            assert!(
                found[0].message.contains("embed_on_modal_from_n_chunks"),
                "{found:?}"
            );
        }
    }

    /// A param that does nothing any more is run without, and `--check`
    /// says it can go.
    #[test]
    fn a_param_that_does_nothing_is_a_warning() {
        let args = crate::source::RunArgs {
            function: "ingest".into(),
            group_type: Some("slack".into()),
            source_type: None,
        };
        let env = StepEnv::new("slack/ingest".into(), &args, vec![]).unwrap();
        let root = tempfile::tempdir().unwrap();
        let params = serde_json::json!({
            "api": { "channels": ["bridge"] },
            "common": { "always_clear_before_ingest": true },
        });
        let warnings = super::check(&env, params, root.path()).unwrap();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("common.always_clear_before_ingest"),
            "{warnings:?}"
        );
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
