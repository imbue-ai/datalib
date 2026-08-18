//! Parse-and-validate the checked-in example configs under
//! `docs/user/config_examples/`, which are in the current TOML steps
//! format.
//!
//! This test lives in the migration crate for one reason: `SourceConfig`
//! does. The stanza envelope around it is retired, but its `type:`-tagged
//! union is still the only mapping from a source type string to that
//! provider's config schema — which is exactly what layer 2 below needs.
//!
//! Three layers of validation:
//!
//! 1. Each file loads as a `DagConfig` and builds a valid `Graph` —
//!    the same `load → to_specs → Graph::build` chain the runner uses,
//!    so cycle / output-ownership / bad-command errors are caught, not
//!    just TOML syntax.
//! 2. Every `datalib-step download <type>` step's `params` round-trip
//!    into `SourceConfig` (the subcommand's type re-injected as the
//!    serde tag), which is `deny_unknown_fields` — this test fails the
//!    moment a documented knob drifts from the real schema: a
//!    misspelled field, a renamed `type`, or a removed source variant.
//! 3. Every `datalib-step render <type>` step carrying `params`
//!    deserializes into that provider's `<P>RenderConfig` (also
//!    `deny_unknown_fields`).
//!
//! `all_sources.toml` is the important one: it enumerates every source
//! `type` plus both input modes for `email` and `carddav`, so this test
//! doubles as a "did someone add a source without documenting it?" nudge.

use datalib_migrate_config::legacy_stanza::SourceConfig;

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
/// step's `params` table with the subcommand's type re-injected as
/// the `type` tag `SourceConfig` discriminates on.
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

/// Validate a render step's `params` against the provider's
/// `<P>RenderConfig`. Only the types the examples actually give render
/// params to are matched; a new one panics with a pointer here.
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
            check!(datalib_etl_anthropic_config::AnthropicRenderConfig)
        }
        "email" => check!(datalib_etl_email_config::EmailRenderConfig),
        "beeper" => check!(datalib_etl_beeper_config::BeeperRenderConfig),
        "signal_backup" => check!(datalib_etl_signal_config::SignalRenderConfig),
        "perseus" => check!(datalib_etl_perseus_config::PerseusRenderConfig),
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
    for name in ["sample_config.toml", "claude_only.toml", "all_sources.toml"] {
        validate_config(name, &example_config(name));
    }
}

/// Same validation, applied to the manual-e2e live-golden config — which lives
/// OUTSIDE this repo (it names real accounts), in the private dir given by
/// `DATALIB_MANUAL_E2E_DIR`. See `docs/dev/testing.md`.
///
/// `#[ignore]` because it depends on a host path that only exists on the one
/// machine that runs the live golden; it is otherwise cheap — pure parsing, no
/// network, no credentials, no subprocess. That makes it the fast way to catch
/// the config drifting from the provider schemas *without* paying for a live
/// re-bake:
///
/// ```sh
/// bazel test //datalib/backend/migrate_config:config_examples_test \
///     --test_arg=--ignored --test_env=DATALIB_MANUAL_E2E_DIR --test_output=all
/// ```
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
