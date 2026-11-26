//! `datalib-package.toml`: what a package is, the build for each platform,
//! and what it adds to a library — source types, each with its own
//! add-source form, the steps and the probe its programs run, and the
//! applets each of its sources brings.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use datalib_dag::config::Reads;
use serde::{Deserialize, Serialize};

use crate::platform::{ANY, TARGETS};

/// The manifest shape this build reads.
pub const SCHEMA: u32 = 1;

/// The functions a source may run. `ingest` is required; each of the
/// others is a step reading its `inputs`, the ingest's tree unless they
/// say otherwise, written only when the source has a command for it.
/// `embed` is written only when the person asks for search by meaning.
pub const SOURCE_FUNCTIONS: &[&str] = &[
    "ingest",
    "render_markdown",
    "render_time_series",
    "render_occurrences",
    "embed",
];

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
    /// The locks its steps hold, each with the slots the wizard gives its
    /// `[[locks]]` entry when the config has none of that name yet.
    #[serde(default)]
    pub locks: BTreeMap<String, usize>,
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
    /// the package, with how the step reads and what it locks.
    pub commands: BTreeMap<String, StepCommand>,
    /// The command that answers "Check connection" and a picker's "Load"
    /// for a source of this type, the first word a path inside the package
    /// (`docs/dev/step_protocol.md` §"Probe").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
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

/// What one function's step runs, and the `reads` and `locks` the wizard
/// writes on it. A manifest writes either the command alone or a table.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(from = "StepCommandSpelling")]
pub struct StepCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<Reads>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<String>,
    /// What the step reads, each a function of the same source (`ingest`)
    /// or a step id (`<group>/<function>`), or a pattern (`*/embed`).
    /// Empty: a source's step other than `ingest` reads its `ingest`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StepCommandSpelling {
    Bare(String),
    Table(StepCommandTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepCommandTable {
    command: String,
    #[serde(default)]
    reads: Option<Reads>,
    #[serde(default)]
    locks: Vec<String>,
    #[serde(default)]
    inputs: Vec<String>,
}

impl From<StepCommandSpelling> for StepCommand {
    fn from(s: StepCommandSpelling) -> Self {
        match s {
            StepCommandSpelling::Bare(command) => StepCommand {
                command,
                reads: None,
                locks: Vec::new(),
                inputs: Vec::new(),
            },
            StepCommandSpelling::Table(t) => StepCommand {
                command: t.command,
                reads: t.reads,
                locks: t.locks,
                inputs: t.inputs,
            },
        }
    }
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
                "schema {} — this build of Datalib reads schema {SCHEMA}; a newer Datalib may \
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
                    "artifacts: {target:?} is not a platform Datalib ships for ({}, or {ANY})",
                    TARGETS.join(", ")
                );
            }
            if !is_sha256(&a.sha256) {
                bail!("artifacts.{target}: sha256 must be 64 lowercase hex digits");
            }
        }
        check_offer(&self.sources, &self.locks)
    }

    /// The build for `target`: its own, else the one for every platform.
    pub fn artifact_for(&self, target: &str) -> Option<(&str, &Artifact)> {
        self.artifacts
            .get_key_value(target)
            .or_else(|| self.artifacts.get_key_value(ANY))
            .map(|(k, a)| (k.as_str(), a))
    }
}

/// What a program a config's `[[programs]]` entry names offers "Add
/// source", as it prints it when the app runs `<program> --describe`
/// (`docs/dev/step_protocol.md` §"Describe"): sources in a manifest's
/// shape, each command the words that follow the program's own.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    #[serde(default)]
    pub sources: Vec<SourceTemplate>,
    #[serde(default)]
    pub locks: BTreeMap<String, usize>,
    #[serde(default)]
    pub fan_ins: Vec<FanIn>,
}

/// A step that reads one function of every source, by a pattern in its
/// `inputs` (`*/render_markdown`), filed under a group of its own: the
/// grid and search index, the map, the time-series index. The wizard
/// wires a source's step into the fan-ins that read its function where
/// a fan-in names its inputs one by one, and writes the fan-in itself
/// (its group, its step and its applet) where `added_with_a_source` says
/// so and the config has none yet.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FanIn {
    pub group: String,
    /// The group's name, written when the wizard writes the group.
    pub name: String,
    /// The step's function: its id is `<group>/<function>`.
    pub function: String,
    pub command: StepCommand,
    /// The command of the applet that shows what it holds, filed under
    /// the group with the group's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applet: Option<String>,
    #[serde(default)]
    pub added_with_a_source: bool,
}

impl Description {
    pub fn parse(json: &str) -> Result<Description> {
        let d: Description = serde_json::from_str(json).context("the description")?;
        check_offer(&d.sources, &d.locks)?;
        for f in &d.fan_ins {
            if !f.command.inputs.iter().any(|i| i.starts_with("*/")) {
                bail!(
                    "fan-in {}/{}: `inputs` names no pattern",
                    f.group,
                    f.function
                );
            }
            if let Some(lock) = f.command.locks.iter().find(|l| !d.locks.contains_key(*l)) {
                bail!(
                    "fan-in {}/{}: holds lock {lock:?}, which `locks` does not declare",
                    f.group,
                    f.function
                );
            }
        }
        Ok(d)
    }
}

/// The rules a manifest's and a description's sources and locks keep.
fn check_offer(sources: &[SourceTemplate], locks: &BTreeMap<String, usize>) -> Result<()> {
    if let Some((name, _)) = locks.iter().find(|(_, slots)| **slots == 0) {
        bail!("locks.{name}: a lock needs at least one slot");
    }
    {
        for s in sources {
            if !s.commands.contains_key("ingest") {
                bail!("source {:?}: `commands` needs an `ingest`", s.source_type);
            }
            if let Some(f) = s
                .commands
                .keys()
                .find(|f| !SOURCE_FUNCTIONS.contains(&f.as_str()))
            {
                bail!(
                    "source {:?}: function {f:?} is not one a source runs ({})",
                    s.source_type,
                    SOURCE_FUNCTIONS.join(", ")
                );
            }
            for (f, c) in &s.commands {
                if c.command.trim().is_empty() {
                    bail!(
                        "source {:?}: `commands.{f}` names no program",
                        s.source_type
                    );
                }
                if let Some(lock) = c.locks.iter().find(|l| !locks.contains_key(*l)) {
                    bail!(
                        "source {:?}: `commands.{f}` holds lock {lock:?}, which `locks` does \
                         not declare",
                        s.source_type
                    );
                }
            }
            if s.probe.as_deref().is_some_and(|p| p.trim().is_empty()) {
                bail!("source {:?}: `probe` names no program", s.source_type);
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
    }
    Ok(())
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
    fn a_command_is_a_string_or_a_table_with_reads_and_locks() {
        let m = Manifest::parse(WEATHER).unwrap();
        let ingest = &m.sources[0].commands["ingest"];
        assert_eq!(ingest.command, "bin/fetch-weather");
        assert_eq!((ingest.reads, ingest.locks.len()), (None, 0));
        assert_eq!(m.sources[0].probe, None);

        let table = WEATHER
            .replace(
                "commands = { ingest = \"bin/fetch-weather\" }",
                "probe = \"bin/probe-weather\"\n\
                 [sources.commands]\n\
                 ingest = { command = \"bin/fetch-weather\", locks = [\"noaa\"] }\n\
                 render_markdown = { command = \"bin/render\", reads = \"files\" }",
            )
            .replace("[artifacts]", "[locks]\nnoaa = 1\n\n[artifacts]");
        let m = Manifest::parse(&table).unwrap();
        let s = &m.sources[0];
        assert_eq!(s.probe.as_deref(), Some("bin/probe-weather"));
        assert_eq!(s.commands["ingest"].locks, ["noaa"]);
        assert_eq!(s.commands["render_markdown"].reads, Some(Reads::Files));
        assert_eq!(m.locks["noaa"], 1);

        let undeclared = table.replace("[locks]\nnoaa = 1\n", "");
        let e = Manifest::parse(&undeclared).unwrap_err().to_string();
        assert!(e.contains("does not declare"), "{e}");
        let bad_reads = table.replace("reads = \"files\"", "reads = \"sometimes\"");
        assert!(Manifest::parse(&bad_reads).is_err());
    }

    #[test]
    fn a_newer_schema_is_refused_by_name() {
        let e = Manifest::parse(&WEATHER.replace("schema = 1", "schema = 2"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("schema 2"), "{e}");
    }
}
