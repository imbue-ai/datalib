//! The checked-in example configs — `docs/user/config_examples/*.toml` and
//! `configs/dag_example.toml` — have to load as the runner would and plan as
//! this binary would, so the documentation cannot drift from the real
//! schemas. Every `ingest` and `render_markdown` step's params go through
//! `dispatch::plan`, the same parse a sync performs.

use std::path::PathBuf;

use crate::dispatch::{self, Phase};
use crate::function::Function;

fn example_config(repo_rel: &str) -> PathBuf {
    let r = runfiles::Runfiles::create().expect("runfiles tree");
    let rel = format!("_main/{repo_rel}");
    let path = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("rlocation for {rel}"));
    assert!(path.exists(), "example config missing in runfiles: {rel}");
    path
}

/// `name` is only used for panic messages.
fn validate_config(name: &str, path: &std::path::Path) {
    let (cfg, _data_root) = datalib_dag::config::load(path)
        .unwrap_or_else(|e| panic!("{name}: failed to load as a DAG config: {e:#}"));
    let specs =
        datalib_dag::config::to_specs(&cfg).unwrap_or_else(|e| panic!("{name}: to_specs: {e:#}"));
    datalib_dag::Graph::build(specs).unwrap_or_else(|e| panic!("{name}: graph build: {e:#}"));

    let data_root = tempfile::tempdir().expect("tempdir");
    for step in &cfg.steps {
        // A custom command is somebody else's program; this binary only
        // vouches for the steps it will run itself.
        if step.command.is_some() {
            continue;
        }
        let (Some(group), Some(function)) = (step.group.as_deref(), step.function.as_deref())
        else {
            panic!("{name}: step {} has no command and no group", step.id);
        };
        let phase = match Function::parse(function) {
            Some(Function::Ingest) => Phase::Ingest,
            Some(Function::RenderMarkdown) => Phase::Render,
            Some(Function::GridIndex | Function::QmdIndex) => continue,
            None => panic!(
                "{name}: step {}: datalib-step has no function {function:?}",
                step.id
            ),
        };
        let ty = cfg
            .groups
            .iter()
            .find(|g| g.id == group)
            .and_then(|g| g.r#type.as_deref())
            .unwrap_or_else(|| panic!("{name}: step {}: its group declares no type", step.id));
        let params = match &step.params {
            Some(p) => serde_json::to_value(p)
                .unwrap_or_else(|e| panic!("{name}: step {}: params → JSON: {e}", step.id)),
            None => serde_json::json!({}),
        };
        let raw_dir = datalib_etl::layout::ingest_root(data_root.path(), group);
        dispatch::plan(ty, phase, group, raw_dir, params).unwrap_or_else(|e| {
            panic!(
                "{name}: step {}: params don't plan as a {ty} {phase:?} step: {e:#}",
                step.id
            )
        });
    }
}

#[test]
fn example_configs_load_and_plan() {
    for name in [
        "docs/user/config_examples/sample_config.toml",
        "docs/user/config_examples/claude_only.toml",
        "docs/user/config_examples/all_sources.toml",
        // The walkthrough config AGENTS.md sends people to.
        "configs/dag_example.toml",
    ] {
        validate_config(name, &example_config(name));
    }
}

/// Same validation, applied to the manual-e2e live-golden config — which lives
/// OUTSIDE this repo (it names real accounts), in the private dir given by
/// `DATALIB_MANUAL_E2E_DIR`. See `docs/dev/testing.md`.
#[test]
#[ignore]
fn manual_e2e_config_loads_and_plans() {
    let dir = std::env::var("DATALIB_MANUAL_E2E_DIR").expect(
        "set DATALIB_MANUAL_E2E_DIR to the private manual-e2e data dir \
         (the one holding dag.toml + sources/ + snapshots/)",
    );
    let path = std::path::PathBuf::from(dir).join("dag.toml");
    assert!(
        path.exists(),
        "missing {} — expected the DAG-format config in the manual-e2e data dir",
        path.display()
    );
    validate_config("dag.toml", &path);
}
