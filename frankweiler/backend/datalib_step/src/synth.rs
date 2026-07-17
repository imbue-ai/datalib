//! The `synthesize` subcommand: build HTTP playback fixtures for one
//! source, reading its `input_path` (interpreted as a checked-in raw
//! fixture tree) and writing replay tapes into `--out`.
//!
//! Dev utility, not a pipeline step: it writes outside the data root
//! and exists to (re)generate the fixture trees that `download
//! --playback-root` replays in hermetic runs. Ported from
//! `frankweiler-sync --synthesize-playback-root`, one source per
//! invocation.
//!
//! Params are read structurally from the JSON (`source.common.input_path`,
//! `source.fetch_photos`) rather than through the typed provider
//! configs — synthesizers only need the fixture-tree location, and
//! this keeps the dev utility off the normalize/validate path.
//! Sources without an HTTP synthesizer are skipped with a log line,
//! mirroring sync's behavior.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_dag::events::{Event, LogLevel};
use frankweiler_etl::synthesize::Synthesizer;

use crate::events::{Emitter, OutputClaim};

pub fn run(
    step_type: &str,
    name: &str,
    source: &serde_json::Value,
    data_root: &Path,
    out: &Path,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    // input_path, resolved like SourceCommon::resolve_paths: explicit
    // (tilde-expanded) else the canonical raw dir.
    let input: PathBuf = match source
        .pointer("/common/input_path")
        .and_then(|v| v.as_str())
    {
        Some(p) if p.starts_with("~/") => match std::env::var("HOME") {
            Ok(home) => Path::new(&home).join(&p[2..]),
            Err(_) => PathBuf::from(p),
        },
        Some(p) => PathBuf::from(p),
        None => data_root.join(name).join("raw"),
    };
    let log = |msg: String| {
        emitter.event(&Event::Log {
            step: String::new(), // re-tagged by the runner if any
            level: LogLevel::Info,
            msg,
        });
    };

    let synth: Box<dyn Synthesizer> = match step_type {
        "claude_api" | "claude_export" => Box::new(
            frankweiler_etl_anthropic::synthesize::AnthropicSynth::new(input.clone()),
        ),
        "chatgpt_api" => Box::new(frankweiler_etl_chatgpt::synthesize::ChatgptSynth::new(
            input.clone(),
        )),
        "slack_api" => Box::new(frankweiler_etl_slack::synthesize::SlackSynth::new(
            input.clone(),
        )),
        "github_api" => Box::new(frankweiler_etl_github::synthesize::GithubSynth::new(
            input.clone(),
        )),
        "gitlab_api" => Box::new(frankweiler_etl_gitlab::synthesize::GitlabSynth::new(
            input.clone(),
        )),
        "notion_api" => Box::new(frankweiler_etl_notion::synthesize::NotionSynth::new(
            input.clone(),
        )),
        "beeper" => Box::new(frankweiler_etl_beeper::synthesize::BeeperSynth::new(
            input.clone(),
        )),
        // LinkedIn is file-backed except the optional connection-photo
        // fetch; there are playback fixtures to synthesize iff that's
        // enabled.
        "linkedin"
            if source
                .get("fetch_photos")
                .and_then(|v| v.as_bool())
                .unwrap_or(false) =>
        {
            Box::new(frankweiler_etl_linkedin::synthesize::LinkedinSynth::new(
                input.clone(),
            ))
        }
        // Everything else is file-backed / translate-only / synth-less:
        // no extract HTTP to play back. Skip quietly like sync did.
        other => {
            log(format!(
                "synthesize {name} ({other}): skipped (no HTTP synthesizer for this source type)"
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
