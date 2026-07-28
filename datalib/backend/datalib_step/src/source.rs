//! Parsing of the runner-appended step declaration flags.
//!
//! With per-provider step types the params carry no `type:`
//! discriminator (the nested subcommand names the provider) and
//! no `name:` either — `--params` is the provider's own config
//! subtree verbatim, deserialized by [`crate::dispatch::plan`], and
//! the source name is derived from the step's declared outputs: the
//! first path component of the first `--outputs` entry (`slack/raw` →
//! `slack`), which is exactly the `<name>/…` directory prefix the
//! step writes under.

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

/// The source name this step works under, derived from `--outputs`:
/// the first path component of the first declared output.
pub fn name_from_outputs(outputs: Option<&str>) -> Result<String> {
    let outputs = outputs.context(
        "download/render need `outputs:` declared in the DAG config \
         (passed as --outputs) to derive the source name",
    )?;
    let outs: Vec<String> =
        serde_json::from_str(outputs).context("parse --outputs as a JSON string array")?;
    let first = outs
        .first()
        .context("--outputs is empty — declare e.g. `outputs: [slack/raw]`")?;
    let name = first.split('/').next().unwrap_or_default();
    anyhow::ensure!(
        !name.trim().is_empty() && !name.contains('*'),
        "cannot derive a source name from output {first:?} \
         (expected `<name>/raw` or `<name>/rendered_md`)"
    );
    Ok(name.to_string())
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
    fn name_comes_from_the_first_output() {
        assert_eq!(
            name_from_outputs(Some(r#"["slack/raw"]"#)).unwrap(),
            "slack"
        );
        assert_eq!(
            name_from_outputs(Some(r#"["slack/rendered_md","other/x"]"#)).unwrap(),
            "slack"
        );
    }

    #[test]
    fn underivable_names_are_rejected() {
        assert!(name_from_outputs(None).is_err());
        assert!(name_from_outputs(Some("[]")).is_err());
        assert!(name_from_outputs(Some(r#"["**/rendered_md"]"#)).is_err());
        assert!(name_from_outputs(Some(r#"["/abs/path"]"#)).is_err());
        assert!(name_from_outputs(Some("not json")).is_err());
    }
}
