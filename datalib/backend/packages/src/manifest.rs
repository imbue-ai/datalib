//! `datalib-package.toml`: what a package is, the build for each platform,
//! and what it adds to a library — source types, each with its own
//! add-source form and the applets each of its sources brings.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::platform::{ANY, TARGETS};

/// The manifest shape this build reads.
pub const SCHEMA: u32 = 1;

/// The functions a package source may run. `ingest` is required; each of
/// the others is a step reading the ingest's tree, written only when the
/// package has a program for it.
pub const SOURCE_FUNCTIONS: &[&str] = &["ingest", "render_markdown", "render_time_series"];

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// The step protocol version the package was written against
    /// (`docs/dev/step_protocol.md`).
    #[serde(default)]
    pub step_protocol: Option<u32>,
    /// One `.tar.gz` per target triple, or one for [`ANY`].
    pub artifacts: BTreeMap<String, Artifact>,
    #[serde(default)]
    pub sources: Vec<SourceTemplate>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Relative to the manifest's own URL, or absolute.
    pub url: String,
    pub sha256: String,
    /// Bytes, for the review screen. Optional: the hash is the check.
    #[serde(default)]
    pub size: Option<u64>,
}

/// A source type the package adds to "Add source".
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTemplate {
    /// The group `type` a source of this kind gets.
    #[serde(rename = "type")]
    pub source_type: String,
    /// Function → the command its step runs, the first word a path inside
    /// the package.
    pub commands: BTreeMap<String, String>,
    /// The add-source form, in the shape of the UI's `CatalogEntry`
    /// (`datalib/ui/src/config/catalog.ts`) without `type`: `label`,
    /// `blurb`, `sections`, `fields` and the rest. The UI draws it with the
    /// same code as every built-in source.
    pub wizard: toml::Table,
    /// The applets each source of this type gets, written into the config
    /// with the source and removed with it.
    #[serde(default)]
    pub applets: Vec<AppletTemplate>,
}

/// An applet a package source brings. `id` is the applet's name within the
/// source; the config's id is composed from the source's and this.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppletTemplate {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub params: Option<toml::Table>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest> {
        let m: Manifest = toml::from_str(text).context("datalib-package.toml")?;
        m.check()?;
        Ok(m)
    }

    fn check(&self) -> Result<()> {
        if self.schema != SCHEMA {
            bail!(
                "schema {} — this build of datalib reads schema {SCHEMA}; a newer datalib may \
                 read this package",
                self.schema
            );
        }
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            bail!("`name` and `version` must not be empty");
        }
        if self.artifacts.is_empty() {
            bail!("`artifacts` names no build");
        }
        for (target, a) in &self.artifacts {
            if target != ANY && !TARGETS.contains(&target.as_str()) {
                bail!(
                    "artifacts: {target:?} is not a platform datalib ships for ({}, or {ANY})",
                    TARGETS.join(", ")
                );
            }
            if !is_sha256(&a.sha256) {
                bail!("artifacts.{target}: sha256 must be 64 lowercase hex digits");
            }
        }
        for s in &self.sources {
            if !s.commands.contains_key("ingest") {
                bail!("source {:?}: `commands` needs an `ingest`", s.source_type);
            }
            if let Some(f) = s
                .commands
                .keys()
                .find(|f| !SOURCE_FUNCTIONS.contains(&f.as_str()))
            {
                bail!(
                    "source {:?}: function {f:?} is not one a package source runs ({})",
                    s.source_type,
                    SOURCE_FUNCTIONS.join(", ")
                );
            }
            if !s.wizard.get("label").is_some_and(|v| v.is_str()) {
                bail!("source {:?}: `wizard.label` is required", s.source_type);
            }
            if let Some(a) = s.applets.iter().find(|a| !is_identifier(&a.id)) {
                bail!(
                    "source {:?}: applet id {:?} must be letters, digits and `_`",
                    s.source_type,
                    a.id
                );
            }
            if s.wizard.contains_key("type") {
                bail!(
                    "source {:?}: `wizard.type` is the source's own `type`; leave it out",
                    s.source_type
                );
            }
        }
        Ok(())
    }

    /// The build for `target`: its own, else the one for every platform.
    pub fn artifact_for(&self, target: &str) -> Option<(&str, &Artifact)> {
        self.artifacts
            .get_key_value(target)
            .or_else(|| self.artifacts.get_key_value(ANY))
            .map(|(k, a)| (k.as_str(), a))
    }
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const WEATHER: &str = r#"
schema = 1
name = "Weather"
version = "1.2.0"
description = "Hourly observations from a NOAA station"

[artifacts]
any = { url = "weather.tar.gz", sha256 = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824" }

[[sources]]
type = "weather"
commands = { ingest = "bin/fetch-weather" }
[sources.wizard]
label = "Weather station"
blurb = "Hourly observations from a NOAA station."
"#;

    #[test]
    fn a_good_manifest_parses() {
        let m = Manifest::parse(WEATHER).unwrap();
        assert_eq!(m.sources[0].source_type, "weather");
        assert_eq!(m.artifact_for("aarch64-apple-darwin").unwrap().0, "any");
    }

    #[test]
    fn a_platform_datalib_does_not_ship_is_refused() {
        let bad = WEATHER.replace("any = {", "x86_64-pc-windows-msvc = {");
        let e = Manifest::parse(&bad).unwrap_err().to_string();
        assert!(e.contains("not a platform"), "{e}");
    }

    #[test]
    fn a_source_needs_an_ingest_and_a_label() {
        let no_ingest = WEATHER.replace("ingest = ", "render_markdown = ");
        assert!(Manifest::parse(&no_ingest).is_err());
        let no_label = WEATHER.replace("label = \"Weather station\"", "");
        assert!(Manifest::parse(&no_label).is_err());
    }

    #[test]
    fn an_applet_belongs_to_a_source() {
        let with =
            format!("{WEATHER}[[sources.applets]]\nid = \"chart\"\ncommand = \"bin/chart\"\n");
        let m = Manifest::parse(&with).unwrap();
        assert_eq!(m.sources[0].applets[0].id, "chart");
        let top_level =
            format!("{WEATHER}\n[[applets]]\nid = \"chart\"\ncommand = \"bin/chart\"\n");
        assert!(Manifest::parse(&top_level).is_err());
        let bad_id = with.replace("id = \"chart\"", "id = \"a-chart\"");
        assert!(Manifest::parse(&bad_id).is_err());
    }

    #[test]
    fn a_newer_schema_is_refused_by_name() {
        let e = Manifest::parse(&WEATHER.replace("schema = 1", "schema = 2"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("schema 2"), "{e}");
    }
}
