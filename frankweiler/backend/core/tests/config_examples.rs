//! Parse-and-validate the checked-in example configs under
//! `docs/user/config_examples/`.
//!
//! `SourceConfig` is `#[serde(deny_unknown_fields)]`, so this test fails
//! the moment a documented stanza drifts from the real schema — a
//! misspelled field, a renamed `type:`, or a removed source variant. It
//! also exercises `Config::validate()` (the Notion/Yolink rules), so the
//! example values have to be internally consistent, not just well-typed.
//!
//! `all_sources.yaml` is the important one: it enumerates every source
//! `type` plus both input modes for `email` and `carddav`, so this test
//! doubles as a "did someone add a source without documenting it?" nudge.

use frankweiler_core::config::load_config;

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

#[test]
fn example_configs_parse_and_validate() {
    for name in ["sample_config.yaml", "claude_only.yaml", "all_sources.yaml"] {
        let path = example_config(name);
        load_config(Some(&path))
            .unwrap_or_else(|e| panic!("{name} failed to load against the config schema: {e}"));
    }
}
