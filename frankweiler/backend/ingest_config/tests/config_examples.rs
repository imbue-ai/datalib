//! Parse-and-validate the checked-in example configs under
//! `docs/user/config_examples/`, which are in the DAG steps format.
//!
//! Three layers of validation:
//!
//! 1. Each file loads as a `DagConfig` and builds a valid `Graph` —
//!    the same `load → to_specs → Graph::build` chain the runner uses,
//!    so cycle / output-ownership / bad-command errors are caught, not
//!    just YAML syntax.
//! 2. Every `datalib-step download <type>` step's `params:` round-trip
//!    into `SourceConfig` (the subcommand's type re-injected as the
//!    serde tag), which is `deny_unknown_fields` — this test fails the
//!    moment a documented knob drifts from the real schema: a
//!    misspelled field, a renamed `type:`, or a removed source variant.
//! 3. Every `datalib-step render <type>` step carrying `params:`
//!    deserializes into that provider's `<P>RenderConfig` (also
//!    `deny_unknown_fields`).
//!
//! `all_sources.yaml` is the important one: it enumerates every source
//! `type` plus both input modes for `email` and `carddav`, so this test
//! doubles as a "did someone add a source without documenting it?" nudge.

use frankweiler_ingest_config::SourceConfig;

/// Resolve a `docs/user/config_examples/<name>` file from the test's runfiles
/// tree (declared as a `data` dep in BUILD.bazel). Mirrors the runfiles
/// lookup in `fixture_db_snapshot.rs`.
fn example_config(name: &str) -> std::path::PathBuf {
    let r = runfiles::Runfiles::create().expect("runfiles tree");
    let rel = format!("_main/docs/user/config_examples/{name}");
    let path = r
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("rlocation for {rel}"));
    assert!(path.exists(), "example config missing in runfiles: {rel}");
    path
}

/// `"datalib-step download slack_api"` → `Some(("download", "slack_api"))`.
/// Non-`datalib-step` commands and the source-independent subcommands
/// (`grid_index`, `qmd_index`) return `None`.
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

/// Rebuild the value `datalib-step download <ty>` deserializes: the
/// step's `params:` mapping with the subcommand's type re-injected as
/// the `type:` tag `SourceConfig` discriminates on.
fn params_with_type(ty: &str, params: Option<&serde_yaml::Value>) -> serde_yaml::Value {
    let mut m = serde_yaml::Mapping::new();
    m.insert("type".into(), ty.into());
    if let Some(serde_yaml::Value::Mapping(p)) = params {
        for (k, v) in p {
            m.insert(k.clone(), v.clone());
        }
    }
    serde_yaml::Value::Mapping(m)
}

/// Validate a render step's `params:` against the provider's
/// `<P>RenderConfig`. Only the types the examples actually give render
/// params to are matched; a new one panics with a pointer here.
fn validate_render_params(file: &str, id: &str, ty: &str, params: &serde_yaml::Value) {
    macro_rules! check {
        ($t:ty) => {{
            let _: $t = serde_yaml::from_value(params.clone()).unwrap_or_else(|e| {
                panic!(
                    "{file}: step {id}: render params don't match {}: {e}",
                    stringify!($t)
                )
            });
        }};
    }
    match ty {
        "claude_api" | "claude_export" => {
            check!(frankweiler_etl_anthropic_config::AnthropicRenderConfig)
        }
        "email" => check!(frankweiler_etl_email_config::EmailRenderConfig),
        "beeper" => check!(frankweiler_etl_beeper_config::BeeperRenderConfig),
        "signal_backup" => check!(frankweiler_etl_signal_config::SignalRenderConfig),
        "perseus" => check!(frankweiler_etl_perseus_config::PerseusRenderConfig),
        other => panic!(
            "{file}: step {id}: render params present for type {other} — \
             add a match arm (and BUILD dep) in config_examples.rs"
        ),
    }
}

#[test]
fn example_configs_parse_and_validate() {
    for name in ["sample_config.yaml", "claude_only.yaml", "all_sources.yaml"] {
        let path = example_config(name);
        let (cfg, _data_root) = frankweiler_dag::config::load(&path)
            .unwrap_or_else(|e| panic!("{name}: failed to load as a DAG config: {e:#}"));
        let specs = frankweiler_dag::config::to_specs(&cfg)
            .unwrap_or_else(|e| panic!("{name}: to_specs: {e:#}"));
        frankweiler_dag::Graph::build(specs)
            .unwrap_or_else(|e| panic!("{name}: graph build: {e:#}"));

        for step in &cfg.steps {
            let Some((phase, ty)) = step_phase_and_type(&step.command) else {
                continue;
            };
            match phase {
                "download" => {
                    let v = params_with_type(ty, step.params.as_ref());
                    let _: SourceConfig = serde_yaml::from_value(v).unwrap_or_else(|e| {
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
}
