//! Parse-and-validate the checked-in example configs — those under
//! `docs/user/config_examples/` plus `configs/dag_example.toml` — which
//! are in the current TOML steps format.

use datalib_migrate_config::legacy_stanza::SourceConfig;

fn example_config(repo_rel: &str) -> std::path::PathBuf {
    let r = runfiles::Runfiles::create().expect("runfiles tree");
    let rel = format!("_main/{repo_rel}");
    let path = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("rlocation for {rel}"));
    assert!(path.exists(), "example config missing in runfiles: {rel}");
    path
}

fn step_phase_and_type(command: &str) -> Option<(&str, &str)> {
    let mut words = command.split_whitespace();
    if words.next()? != "datalib-step" {
        return None;
    }
    match words.next()? {
        phase @ ("download" | "render") => Some((phase, words.next()?)),
        _ => None,
    }
}

fn params_with_type(ty: &str, params: Option<&toml::Value>) -> toml::Value {
    let mut m = toml::Table::new();
    m.insert("type".into(), ty.into());
    if let Some(toml::Value::Table(p)) = params {
        for (k, v) in p {
            m.insert(k.clone(), v.clone());
        }
    }
    toml::Value::Table(m)
}

fn validate_render_params(file: &str, id: &str, ty: &str, params: &toml::Value) {
    macro_rules! check {
        ($t:ty) => {{
            let _: $t = params.clone().try_into().unwrap_or_else(|e| {
                panic!(
                    "{file}: step {id}: render params don't match {}: {e}",
                    stringify!($t)
                )
            });
        }};
    }
    match ty {
        "claude_api" | "claude_export" => {
            check!(datalib_etl_claude_config::ClaudeRenderConfig)
        }
        "email" => check!(datalib_etl_email_config::EmailRenderConfig),
        "beeper" => check!(datalib_etl_beeper_config::BeeperRenderConfig),
        "signal_backup" => check!(datalib_etl_signal_config::SignalRenderConfig),
        "perseus" => check!(datalib_etl_perseus_config::PerseusRenderConfig),
        "pdf" => check!(datalib_etl_pdf_config::PdfRenderConfig),
        other => panic!(
            "{file}: step {id}: render params present for type {other} — \
             add a match arm (and BUILD dep) in config_examples.rs"
        ),
    }
}

/// The three validation layers from the module header, applied to one config
/// file. `name` is only used for panic messages.
fn validate_config(name: &str, path: &std::path::Path) {
    let (cfg, _data_root) = datalib_dag::config::load(path)
        .unwrap_or_else(|e| panic!("{name}: failed to load as a DAG config: {e:#}"));
    let specs =
        datalib_dag::config::to_specs(&cfg).unwrap_or_else(|e| panic!("{name}: to_specs: {e:#}"));
    datalib_dag::Graph::build(specs).unwrap_or_else(|e| panic!("{name}: graph build: {e:#}"));

    for step in &cfg.steps {
        let Some((phase, ty)) = step_phase_and_type(&step.command) else {
            continue;
        };
        match phase {
            "download" => {
                let v = params_with_type(ty, step.params.as_ref());
                let _: SourceConfig = v.try_into().unwrap_or_else(|e| {
                    panic!(
                        "{name}: step {}: download params don't match the \
                         {ty} config schema: {e}",
                        step.id
                    )
                });
            }
            "render" => {
                if let Some(params) = step.params.as_ref() {
                    validate_render_params(name, &step.id, ty, params);
                }
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn example_configs_parse_and_validate() {
    for name in [
        "docs/user/config_examples/sample_config.toml",
        "docs/user/config_examples/claude_only.toml",
        "docs/user/config_examples/all_sources.toml",
        // The walkthrough config AGENTS.md sends people to. It is not
        // under docs/, and it was outside this test until a `label =`
        // key landed in it.
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
fn manual_e2e_config_parses_and_validates() {
    let dir = std::env::var("DATALIB_MANUAL_E2E_DIR").expect(
        "set DATALIB_MANUAL_E2E_DIR to the private manual-e2e data dir \
         (the one holding dag.toml + sources/ + snapshots/)",
    );
    let path = std::path::PathBuf::from(dir).join("dag.toml");
    assert!(
        path.exists(),
        "missing {} — expected the DAG-format config in the manual-e2e data dir. \
         If that dir still holds a pre-TOML dag.yaml, convert it once: \
         `datalib-migrate-config <dir>/dag.yaml -o <dir>/dag.toml`",
        path.display()
    );
    validate_config("dag.toml", &path);
}
