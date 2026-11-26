//! `datalib-step --describe`: the sources this program offers "Add source",
//! printed as one JSON `Description` (`datalib_packages::manifest`), the
//! shape a package manifest's `[[sources]]` has, with the locks and the
//! fan-ins their steps need. The forms are data, in `sources.toml`; which
//! steps a source gets, and how each reads and locks, come from the source
//! type and the function (`step_entry.rs`), so the wizard writes what
//! `examples --append` writes.

use std::collections::BTreeMap;

use anyhow::{bail, Context as _, Result};
use datalib_dag::config::Reads;
use datalib_packages::manifest::{Description, FanIn, SourceTemplate, StepCommand};
use serde::Deserialize;

use crate::function::Function;
use crate::source_type::SourceType;
use crate::step_entry;

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
        locks: step_entry::LOCKS
            .iter()
            .map(|(name, slots)| (name.to_string(), *slots))
            .collect(),
        fan_ins: fan_ins(),
    })
}

/// The group the grid and search index and the map are filed under.
const DOCUMENT_INDEX: &str = "unified_grid_and_search_index";

/// The step every source's `embed` reads: the keyword pass cuts what it
/// embeds.
fn document_index_step() -> String {
    format!("{DOCUMENT_INDEX}/{}", Function::UnifiedGridAndSearchIndex)
}

/// The functions a source of `source_type` has a step for. A time-series
/// source's markdown is daily summaries of numbers, which the keyword
/// index finds whole, so it has no `embed`.
fn functions(source_type: SourceType) -> Vec<Function> {
    let mut out = vec![Function::Ingest, Function::RenderMarkdown];
    if source_type.renders_time_series() {
        out.push(Function::RenderTimeSeries);
    } else {
        out.push(Function::Embed);
    }
    if source_type.renders_occurrences() {
        out.push(Function::RenderOccurrences);
    }
    out
}

/// The command each function's step runs for a source of `source_type`,
/// as the words after `datalib-step`, with how it reads and what it
/// locks.
fn commands(source_type: SourceType) -> BTreeMap<String, StepCommand> {
    let t = source_type.as_str();
    functions(source_type)
        .into_iter()
        .map(|function| {
            let inputs = match function {
                Function::Ingest => Vec::new(),
                Function::Embed => vec![document_index_step()],
                _ => vec![Function::Ingest.as_str().to_string()],
            };
            (
                function.as_str().to_string(),
                step(function, Some(t), &format!("run {function} {t}"), inputs),
            )
        })
        .collect()
}

fn step(
    function: Function,
    group_type: Option<&str>,
    words: &str,
    inputs: Vec<String>,
) -> StepCommand {
    StepCommand {
        command: words.to_string(),
        reads: step_entry::reads(function, group_type)
            .map(|r| Reads::parse(r).expect("step_entry::reads names a `reads` the runner knows")),
        locks: step_entry::lock(function, group_type)
            .map(str::to_string)
            .into_iter()
            .collect(),
        inputs,
    }
}

/// The steps that read one function of every source.
fn fan_ins() -> Vec<FanIn> {
    let fan_in = |group: &str, name: &str, function: Function, reads: Function, applet| FanIn {
        group: group.to_string(),
        name: name.to_string(),
        function: function.as_str().to_string(),
        command: step(
            function,
            None,
            &format!("run {function}"),
            vec![format!("*/{reads}")],
        ),
        applet,
        added_with_a_source: group == step_entry::TIME_SERIES_INDEX_GROUP,
    };
    vec![
        fan_in(
            DOCUMENT_INDEX,
            "Grid and Search Index",
            Function::UnifiedGridAndSearchIndex,
            Function::RenderMarkdown,
            Some(format!("datalib-applet {DOCUMENT_INDEX}")),
        ),
        fan_in(
            DOCUMENT_INDEX,
            "Grid and Search Index",
            Function::EmbeddingMap,
            Function::Embed,
            None,
        ),
        fan_in(
            step_entry::TIME_SERIES_INDEX_GROUP,
            "Time Series Index",
            Function::TimeSeriesIndex,
            Function::RenderTimeSeries,
            Some(format!(
                "datalib-applet {}",
                step_entry::TIME_SERIES_INDEX_GROUP
            )),
        ),
    ]
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

    /// The steps and their scheduling are what `examples --append`
    /// writes: a time-series source renders its series and has no embed,
    /// a calendar lists its occurrences, an embed reads the index's files
    /// under the embedder lock, and Perseus's render reads files.
    #[test]
    fn each_source_gets_the_steps_its_type_has() {
        let d = description().unwrap();
        let of = |t: &str| {
            &d.sources
                .iter()
                .find(|s| s.source_type == t)
                .unwrap()
                .commands
        };
        let fns = |t: &str| of(t).keys().cloned().collect::<Vec<_>>();
        assert_eq!(
            fns("garmin"),
            ["ingest", "render_markdown", "render_time_series"]
        );
        assert_eq!(
            fns("calendar"),
            ["embed", "ingest", "render_markdown", "render_occurrences"]
        );
        let embed = &of("slack")["embed"];
        assert_eq!(embed.command, "run embed slack");
        assert_eq!(
            (embed.reads, embed.locks.as_slice(), embed.inputs.as_slice()),
            (
                Some(Reads::Files),
                ["embedder".to_string()].as_slice(),
                [document_index_step()].as_slice()
            )
        );
        assert_eq!(of("slack")["render_markdown"].reads, Some(Reads::Pinned));
        assert_eq!(of("slack")["render_markdown"].inputs, ["ingest"]);
        assert_eq!(of("perseus")["render_markdown"].reads, Some(Reads::Files));
        assert_eq!(of("slack")["ingest"].reads, None);
        let ts = d
            .fan_ins
            .iter()
            .find(|f| f.function == "time_series_index")
            .unwrap();
        assert!(ts.added_with_a_source);
        assert_eq!(ts.command.inputs, ["*/render_time_series"]);
        assert_eq!(ts.command.locks, ["index"]);
        assert_eq!(d.locks["embedder"], 1);
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
