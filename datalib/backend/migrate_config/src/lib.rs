//! `datalib-migrate-config` — convert a pre-TOML `config.yaml` into the
//! `config.toml` the pipeline reads today.
//!
//! This crate exists so the shipping programs don't have to. `datalib-dag`
//! and `datalib-http` know exactly one config format; every legacy schema,
//! and the only YAML parser left in the tree, lives here. That keeps the
//! runner's config module a single `toml::from_str` and means a format we
//! stopped writing years ago can't affect what a running pipeline accepts.
//!
//! Two legacy formats are recognized, and which one a file is gets decided
//! by its content rather than asked of the user — see [`LegacyFormat`].

pub mod convert;
pub mod legacy_stanza;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The two shapes a pre-TOML `config.yaml` can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyFormat {
    /// The current steps schema, written in YAML — a `steps:` list of
    /// step entries. Converting is a reserialization.
    Steps,
    /// The retired stanza schema — a `sources:` list, one entry per
    /// source, with the provider named by a `type:` tag. Converting is a
    /// real translation into download/render step pairs.
    Stanza,
}

/// Which legacy shape this text is, from the top-level key it uses. The
/// two formats are disjoint on `steps:` / `sources:`, so this needs no
/// guessing — and a file that is neither (or isn't YAML at all) is an
/// error rather than a silent empty conversion.
pub fn detect(text: &str) -> Result<LegacyFormat> {
    // Checked before anything else: running the migrator twice is the
    // likeliest mistake, and the errors it would otherwise produce are
    // baffling (TOML is usually not valid YAML at all, and when it is
    // — `data_root = "x"` — it reads as a bare scalar). No legacy
    // config can be mistaken for TOML in the other direction: `steps:`
    // and `sources:` are not TOML key-value syntax.
    //
    // `is_toml` and not the full loader: an already-converted config
    // that has a *problem* in it is still already converted, and
    // sending it to the YAML parser would bury that problem under a
    // parse error about a file that was never YAML.
    if !text.trim().is_empty() && datalib_dag::config::is_toml(text) {
        bail!("this config is already TOML — there is nothing to migrate");
    }
    let v: serde_yaml::Value =
        serde_yaml::from_str(text).context("parse the legacy config as YAML")?;
    let Some(m) = v.as_mapping() else {
        bail!("not a config: expected a YAML mapping at the top level");
    };
    match (m.contains_key("steps"), m.contains_key("sources")) {
        (true, _) => Ok(LegacyFormat::Steps),
        (false, true) => Ok(LegacyFormat::Stanza),
        (false, false) => bail!("not a datalib config: no top-level `steps:` or `sources:` key"),
    }
}

/// Convert legacy YAML config text to TOML, detecting the format.
///
/// The result is verified before it is returned: it must re-parse as a
/// `DagConfig` and build a valid graph, the same chain the runner runs.
/// A conversion that produced something the runner would reject is a bug
/// in this tool, and it should surface here rather than at the user's
/// next sync.
pub fn convert(text: &str) -> Result<String> {
    let format = detect(text)?;
    let out = match format {
        LegacyFormat::Steps => convert::steps_yaml_to_toml(text),
        LegacyFormat::Stanza => convert::stanza_yaml_to_toml(text),
    }?;
    // The conversion is value-level, so invariants the legacy loader
    // enforced (duplicate source names, say) surface here rather than
    // at parse time. Don't guess whose fault it is: report what the
    // runner rejected and let the message speak.
    verify(&out).context("the converted config does not load")?;
    Ok(out)
}

/// Re-parse converted TOML through the runner's own load → specs →
/// graph chain.
fn verify(toml_text: &str) -> Result<()> {
    let cfg = datalib_dag::config::parse(toml_text)?;
    let specs = datalib_dag::config::to_specs(&cfg)?;
    datalib_dag::Graph::build(specs)?;
    Ok(())
}

/// Where a legacy config lives, given whatever the user pointed us at.
///
/// A data root is the common case (`datalib-migrate-config ~/datalib`),
/// so a directory resolves to the `config.yaml` inside it; anything else
/// is taken as the config file itself.
pub fn resolve_input(arg: &Path) -> PathBuf {
    if arg.is_dir() {
        arg.join("config.yaml")
    } else {
        arg.to_path_buf()
    }
}

/// Where the converted config should land for a given input: `config.toml`
/// beside it, which for the data-root case is exactly where the pipeline
/// looks.
pub fn default_output(input: &Path) -> PathBuf {
    input.with_file_name("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANZA: &str = "data_root: /tmp/dl\nsources:\n  - name: slack\n    \
                          source:\n      type: slack_api\n      sync: {channels: [chat-qi]}\n";
    const STEPS: &str = "steps:\n  - id: slack.download\n    \
                         command: datalib-step download slack_api\n    outputs: [slack/raw]\n";

    #[test]
    fn detects_both_legacy_shapes() {
        assert_eq!(detect(STANZA).unwrap(), LegacyFormat::Stanza);
        assert_eq!(detect(STEPS).unwrap(), LegacyFormat::Steps);
    }

    /// Running the migrator twice is the likeliest mistake, so an
    /// already-converted config says exactly that rather than
    /// surfacing a YAML syntax error from a file that isn't YAML.
    #[test]
    fn an_already_migrated_config_says_so() {
        for toml in [
            "[[steps]]\nid = \"x/raw\"\ncommand = \"c\"\n",
            // Valid YAML *and* valid TOML — the ambiguous case.
            "data_root = \"/tmp/x\"\n",
        ] {
            let err = detect(toml).unwrap_err().to_string();
            assert!(err.contains("already TOML"), "{toml:?} -> {err}");
        }
    }

    #[test]
    fn empty_and_unrelated_yaml_are_rejected() {
        assert!(detect("").is_err());
        assert!(detect("hello: world\n").is_err());
    }

    /// Both conversions produce TOML the runner actually accepts —
    /// `convert` verifies that itself, so reaching `unwrap` proves it.
    #[test]
    fn both_formats_convert_to_loadable_toml() {
        let out = convert(STANZA).unwrap();
        assert!(out.contains("[[steps]]"), "{out}");
        assert!(out.contains("[steps.params.sync]"), "{out}");
        // Ids come out in the current spelling: the tree the step
        // writes, not the legacy `<name>.<phase>`.
        assert!(out.contains(r#"id = "slack/raw""#), "{out}");
        assert!(out.contains(r#"id = "slack/rendered_md""#), "{out}");
        assert!(!out.contains("slack.download"), "{out}");

        let out = convert(STEPS).unwrap();
        assert!(out.contains(r#"id = "slack/raw""#), "{out}");
    }

    /// YAML anchors have no TOML equivalent, so a shared params subtree
    /// has to come out as two independent copies.
    #[test]
    fn anchors_are_expanded() {
        let out = convert(
            "steps:\n  - id: a.download\n    command: c\n    outputs: [a/raw]\n    \
             params: &p\n      sync: {channels: [x]}\n  - id: b.download\n    command: c\n    \
             outputs: [b/raw]\n    params: *p\n",
        )
        .unwrap();
        assert_eq!(out.matches(r#"channels = ["x"]"#).count(), 2, "{out}");
    }

    /// TOML has no null, so a legacy config carrying one is refused with
    /// the offending key path rather than silently losing it.
    #[test]
    fn yaml_nulls_are_refused_with_their_path() {
        let err = format!(
            "{:#}",
            convert("steps:\n  - id: x\n    command: c\n    params: {k: null}\n").unwrap_err()
        );
        assert!(err.contains("params.k"), "{err}");
    }

    /// An unmanaged source (no `sync:` ⇒ no download step) renders
    /// straight from its staged tree, so `common.input_path` has to
    /// land on the render step. Dropping it would point the render at
    /// an empty `<name>/raw` and silently produce nothing — a failure
    /// the graph check can't see, because a param-less render step is
    /// perfectly valid.
    #[test]
    fn an_unmanaged_source_keeps_its_input_path() {
        let out = convert(
            "sources:\n  - name: claude-export\n    source:\n      type: claude_export\n      \
             common:\n        input_path: ~/backups/claude-export\n",
        )
        .unwrap();
        assert!(out.contains(r#"id = "claude-export/rendered_md""#), "{out}");
        assert!(
            out.contains(r#"input_path = "~/backups/claude-export""#),
            "input_path dropped:\n{out}"
        );
        // ...and it stays render-only: no download step to consume it.
        assert!(!out.contains(r#"id = "claude-export/raw""#), "{out}");
    }

    /// A managed source keeps `input_path` on the *download* step,
    /// where the download consumes it; the render reads the raw store.
    #[test]
    fn a_managed_source_keeps_input_path_on_the_download_step() {
        let out = convert(
            "sources:\n  - name: mail\n    source:\n      type: email\n      \
             common:\n        input_path: ~/m.mbox\n      sync:\n        \
             hostname: api.fastmail.com\n",
        )
        .unwrap();
        let dl = out.find(r#"id = "mail/raw""#).unwrap();
        let rn = out.find(r#"id = "mail/rendered_md""#).unwrap();
        let input_at = out.find("input_path").unwrap();
        assert!(dl < input_at && input_at < rn, "input_path moved:\n{out}");
    }

    #[test]
    fn a_directory_resolves_to_the_config_inside_it() {
        let td = tempfile::tempdir().unwrap();
        assert_eq!(resolve_input(td.path()), td.path().join("config.yaml"));
        // A file path is taken as-is, whatever it's called.
        let f = td.path().join("old.yml");
        std::fs::write(&f, "steps: []\n").unwrap();
        assert_eq!(resolve_input(&f), f);
        assert_eq!(default_output(&f), td.path().join("config.toml"));
    }
}
