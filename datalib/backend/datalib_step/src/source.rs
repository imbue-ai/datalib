//! Parsing of the runner-appended step declaration flags.
//!
//! With per-provider step types the params carry no `type:`
//! discriminator (the nested subcommand names the provider) and no
//! `name:` either — `--params` is the provider's own config subtree
//! verbatim, deserialized by [`crate::dispatch::plan`].
//!
//! The source name comes from the step's own id, which the runner puts
//! in `DATALIB_DAG_STEP` and which *is* the tree the step writes
//! (`slack/raw`). Nothing is derived: the step is told where it lives.
//! This used to split the first `--outputs` entry on `/` to reconstruct
//! a prefix and then rebuild the same path from it — see
//! `docs/dev/step_identity.md`.

use anyhow::{Context, Result};

/// The provider config subtree from `--params`. Absent → empty
/// object, so param-less sources (render-only exports) need no
/// `params:` in the config.
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

/// The tree this step owns, relative to the data root: its config `id`,
/// as the runner passes it in `DATALIB_DAG_STEP`.
///
/// Providers still want a bare *name* for their store layout and their
/// commit messages, which is the id's first segment — `slack/raw` →
/// `slack`. That is a display convenience, not identity: nothing
/// resolves anything by it.
pub fn tree_from_env() -> Result<String> {
    let id = std::env::var(STEP_ID_ENV).with_context(|| {
        format!(
            "{STEP_ID_ENV} is not set. `datalib-step` expects to be run by `datalib-dag`, \
             which sets it to the step's config id — the tree the step writes."
        )
    })?;
    anyhow::ensure!(
        !id.trim().is_empty(),
        "{STEP_ID_ENV} is empty; it must be the step's config id"
    );
    Ok(id)
}

/// The runner's name for the environment variable carrying a step's
/// config id. Mirrors `datalib_dag::subprocess::ENV_STEP`; not imported
/// because `datalib-step` deliberately does not depend on the runner.
pub const STEP_ID_ENV: &str = "DATALIB_DAG_STEP";

/// The bare source name for a step id: its first path segment.
pub fn source_name(tree: &str) -> &str {
    tree.split('/').next().unwrap_or(tree)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_parse_verbatim_and_default_empty() {
        let p = parse_params(Some(r#"{"sync":{"media":true}}"#)).unwrap();
        assert_eq!(p["sync"]["media"], true);
        assert!(parse_params(None).unwrap().as_object().unwrap().is_empty());
        // A leftover old-format `type:` tag inside the params is passed
        // through; the provider config's deny_unknown/ignore rules
        // decide its fate downstream, not this layer.
        assert!(parse_params(Some(r#"{"type":"slack_api"}"#)).is_ok());
    }

    #[test]
    fn params_reject_non_objects_and_junk() {
        assert!(parse_params(Some("[1,2]")).is_err());
        assert!(parse_params(Some("not json")).is_err());
    }

    #[test]
    fn source_name_is_the_first_segment() {
        assert_eq!(source_name("slack/raw"), "slack");
        assert_eq!(source_name("slack/rendered_md"), "slack");
        assert_eq!(source_name("work-slack/raw"), "work-slack");
        // A single-segment id is its own name.
        assert_eq!(source_name("solo"), "solo");
    }
}
