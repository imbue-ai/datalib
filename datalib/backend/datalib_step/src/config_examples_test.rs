//! The checked-in example configs — `docs/user/config_examples/*.toml`,
//! `configs/dag_example.toml`, and the TOML the agent config guide shows —
//! have to load as the runner would and plan as
//! this binary would, so the documentation cannot drift from the real
//! schemas. Every `ingest` and `render_markdown` step's params go through
//! `dispatch::plan`, the same parse a sync performs.

use std::path::PathBuf;

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
pub(crate) fn validate_config(name: &str, path: &std::path::Path) {
    let (cfg, _data_root) = datalib_dag::config::load(path)
        .unwrap_or_else(|e| panic!("{name}: failed to load as a DAG config: {e:#}"));
    let specs =
        datalib_dag::config::to_specs(&cfg).unwrap_or_else(|e| panic!("{name}: to_specs: {e:#}"));
    datalib_dag::Graph::build(specs).unwrap_or_else(|e| panic!("{name}: graph build: {e:#}"));

    let data_root = tempfile::tempdir().expect("tempdir");
    let found = crate::check_params::problems(&cfg, data_root.path());
    assert!(
        found.is_empty(),
        "{name}: a sync would refuse these steps' params:\n{}",
        found
            .iter()
            .map(|p| format!("{}: {}", p.step, p.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn example_configs_load_and_plan() {
    for name in [
        "docs/user/config_examples/sample_config.toml",
        "docs/user/config_examples/claude_only.toml",
        "docs/user/config_examples/all_sources.toml",
        // The walkthrough config AGENTS.md sends people to.
        "configs/dag_example.toml",
        // The demo library the docker image bakes in.
        "datalib/docker/demo/config.toml",
    ] {
        validate_config(name, &example_config(name));
    }
}

/// The config guide the app serves to agents (`/agent/config.md`) shows
/// one example config. An agent copies it, so a shape the loader has
/// retired there (it once said `sync = {}`) teaches every agent to write
/// a config that does not load.
#[test]
fn the_agent_config_guides_example_loads_and_plans() {
    let guide = std::fs::read_to_string(example_config(
        "datalib/backend/http/src/agent_config_guide.md",
    ))
    .expect("read the guide");
    let toml = guide
        .split("```toml\n")
        .nth(1)
        .and_then(|rest| rest.split("```").next())
        .expect("the guide has a ```toml block");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, toml).expect("write the example");
    validate_config("agent_config_guide.md", &path);
    let check = datalib_dag::config::check_text(toml);
    assert!(
        !check
            .diagnostics
            .iter()
            .any(|d| d.severity.drops_the_entry()),
        "{:?}",
        check.diagnostics
    );
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
