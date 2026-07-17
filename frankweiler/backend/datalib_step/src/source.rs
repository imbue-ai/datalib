//! Step params parsing: `{"name": …, "source": {…}}`.
//!
//! With per-provider step types, the params carry no `type:`
//! discriminator — the step type names the provider, and `source` is
//! that provider's own config subtree, deserialized by
//! [`crate::dispatch::plan`]. `name` stays orchestrator-owned (it
//! becomes the `<name>/…` directory prefix), exactly as in the old
//! format's source entry.

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepParams {
    pub name: String,
    /// The provider's config subtree (no `type:` tag). Defaults to an
    /// empty object so bare sources (`{"name": "x"}`) parse.
    #[serde(default = "empty_object")]
    pub source: serde_json::Value,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

pub fn parse_params(params_json: &str) -> Result<StepParams> {
    let p: StepParams = serde_json::from_str(params_json)
        .context("parse --params-json as {\"name\": …, \"source\": {…}}")?;
    anyhow::ensure!(!p.name.trim().is_empty(), "params need a non-empty name");
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_source() {
        let p = parse_params(r#"{"name":"slack","source":{"sync":{"media":true}}}"#).unwrap();
        assert_eq!(p.name, "slack");
        assert_eq!(p.source["sync"]["media"], true);
    }

    #[test]
    fn source_defaults_to_empty_and_type_tag_is_not_special() {
        let p = parse_params(r#"{"name":"x"}"#).unwrap();
        assert!(p.source.as_object().unwrap().is_empty());
        // A leftover old-format `type:` tag inside source is passed
        // through; the provider config's deny_unknown/ignore rules
        // decide its fate downstream, not this layer.
        assert!(parse_params(r#"{"name":"x","source":{"type":"slack_api"}}"#).is_ok());
    }

    #[test]
    fn rejects_empty_name_and_unknown_keys() {
        assert!(parse_params(r#"{"name":"  "}"#).is_err());
        assert!(parse_params(r#"{"name":"x","typo":1}"#).is_err());
    }
}
