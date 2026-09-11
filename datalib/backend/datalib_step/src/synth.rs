//! The `synthesize` subcommand: build HTTP playback fixtures for one
//! source, reading a checked-in raw fixture tree (`--params` may name it
//! as `fixture_path`; else the group's ingest tree) and writing replay
//! tapes into `--out`. A dev utility, not a step: it takes the group id
//! from `--name`, not the environment.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_dag::events::{Event, LogLevel};
use datalib_etl::synthesize::Synthesizer;

use crate::events::{Emitter, OutputClaim};
use crate::source_type::SourceType;

pub fn run(
    step_type: &str,
    name: &str,
    source: &serde_json::Value,
    data_root: &Path,
    out: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    // The fixture tree: explicit (tilde-expanded) else the group's
    // ingest tree. A synth-only key, not a method table.
    let input: PathBuf = match source.get("fixture_path").and_then(|v| v.as_str()) {
        Some(p) => datalib_source_common::expand_tilde(Path::new(p)),
        None => datalib_etl::layout::ingest_root(data_root, name),
    };
    let log = |msg: String| {
        emitter.event(&Event::Log {
            step: String::new(), // re-tagged by the runner if any
            level: LogLevel::Info,
            msg,
            target: None,
            fields: None,
        });
    };

    let synth: Box<dyn Synthesizer> = match SourceType::parse(step_type) {
        // Only the API side of claude makes requests; an export ingest
        // has no HTTP to play back, and synthesizes the same tapes.
        Some(SourceType::Claude) => Box::new(datalib_etl_claude::synthesize::ClaudeSynth::new(
            input.clone(),
        )),
        Some(SourceType::Chatgpt) => Box::new(datalib_etl_chatgpt::synthesize::ChatgptSynth::new(
            input.clone(),
        )),
        Some(SourceType::Slack) => Box::new(datalib_etl_slack::synthesize::SlackSynth::new(
            input.clone(),
        )),
        Some(SourceType::Github) => Box::new(datalib_etl_github::synthesize::GithubSynth::new(
            input.clone(),
        )),
        Some(SourceType::Gitlab) => Box::new(datalib_etl_gitlab::synthesize::GitlabSynth::new(
            input.clone(),
        )),
        Some(SourceType::Notion) => Box::new(datalib_etl_notion::synthesize::NotionSynth::new(
            input.clone(),
        )),
        Some(SourceType::Beeper) => Box::new(datalib_etl_beeper::synthesize::BeeperSynth::new(
            input.clone(),
        )),
        // LinkedIn is file-backed except the optional connection-photo
        // fetch; there are playback fixtures to synthesize iff that's
        // enabled.
        Some(SourceType::Linkedin)
            if source
                .pointer("/export/fetch_photos")
                .and_then(|v| v.as_bool())
                .unwrap_or(false) =>
        {
            Box::new(datalib_etl_linkedin::synthesize::LinkedinSynth::new(
                input.clone(),
            ))
        }
        // Everything else is file-backed or otherwise synth-less: no
        // download HTTP to play back. Skip quietly like sync did.
        _ => {
            log(format!(
                "synthesize {name} ({step_type}): skipped (no HTTP synthesizer for this source type)"
            ));
            return Ok(vec![]);
        }
    };

    let report = synth
        .synthesize(out)
        .with_context(|| format!("synthesize {name} ({step_type})"))?;
    log(format!(
        "synthesize {name} ({step_type}): {} fixtures from {} → {}",
        report.fixtures_written,
        input.display(),
        out.display(),
    ));
    Ok(vec![])
}
