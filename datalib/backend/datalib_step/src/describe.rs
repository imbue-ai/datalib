//! `datalib-step --describe`: the sources this program offers "Add source",
//! printed as one JSON `Description` (`datalib_packages::manifest`), the
//! shape a package manifest's `[[sources]]` has. The forms are data, in
//! `sources.toml`; the commands each function's step runs come from the
//! source type.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use datalib_packages::manifest::{Description, SourceTemplate, StepCommand};
use serde::Deserialize;

use crate::source_type::SourceType;

const SOURCES: &str = include_str!("sources.toml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Listed {
    sources: Vec<ListedSource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListedSource {
    #[serde(rename = "type")]
    source_type: String,
    #[serde(default)]
    probe: Option<String>,
    wizard: toml::Table,
}

pub fn description() -> Result<Description> {
    let listed: Listed = toml::from_str(SOURCES).context("sources.toml")?;
    let mut sources = Vec::with_capacity(listed.sources.len());
    for s in listed.sources {
        let Some(source_type) = SourceType::parse(&s.source_type) else {
            bail!("sources.toml: {:?} is not a source type", s.source_type);
        };
        if let Some(probe) = &s.probe {
            if *probe != format!("probe {source_type}") || !crate::probe::probes(source_type) {
                bail!("sources.toml: {source_type} has no probe {probe:?}");
            }
        }
        sources.push(SourceTemplate {
            source_type: s.source_type,
            commands: commands(source_type),
            probe: s.probe,
            wizard: s.wizard,
            applets: Vec::new(),
        });
    }
    Ok(Description {
        sources,
        locks: BTreeMap::new(),
    })
}

/// The command each function's step runs for a source of `source_type`,
/// as the words after `datalib-step`.
fn commands(source_type: SourceType) -> BTreeMap<String, StepCommand> {
    ["ingest", "render_markdown"]
        .into_iter()
        .map(|function| {
            let command = StepCommand {
                command: format!("run {function} {source_type}"),
                reads: None,
                locks: Vec::new(),
            };
            (function.to_string(), command)
        })
        .collect()
}

/// Print the description and exit: what the app reads when a config's
/// `[[programs]]` entry for this program lists `describe`.
#[allow(clippy::disallowed_macros)]
pub fn print() -> ! {
    match description().and_then(|d| Ok(serde_json::to_string(&d)?)) {
        Ok(json) => {
            println!("{json}");
            std::process::exit(0)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn the_description_parses_as_a_program_prints_it() {
        let d = description().unwrap();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(Description::parse(&json).unwrap(), d);
    }

    /// Every type "Add source" can write a step for has an entry: one
    /// missing here would vanish from the picker.
    #[test]
    fn every_source_type_is_offered() {
        let d = description().unwrap();
        let missing: Vec<&SourceType> = SourceType::VARIANTS
            .iter()
            .filter(|t| !d.sources.iter().any(|s| s.source_type == t.as_str()))
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    #[test]
    fn a_source_offers_its_probe_only_where_datalib_step_has_one() {
        let d = description().unwrap();
        let slack = d.sources.iter().find(|s| s.source_type == "slack").unwrap();
        assert_eq!(slack.probe.as_deref(), Some("probe slack"));
        assert_eq!(slack.commands["ingest"].command, "run ingest slack");
        for s in &d.sources {
            if s.probe.is_some() {
                assert!(crate::probe::probes(
                    SourceType::parse(&s.source_type).unwrap()
                ));
            }
        }
    }
}
