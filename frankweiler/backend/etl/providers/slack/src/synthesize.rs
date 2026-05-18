//! Stub: Slack HTTP fixture synthesizer.
//!
//! TODO: walk `slack_api/raw_api/<method>/*.jsonl` event store and emit
//! fixtures for every method the Extract step calls:
//!
//! * `auth.test`, `users.list` — single-call envelopes (modulo `users`
//!   pagination via `response_metadata.next_cursor`).
//! * `conversations.list`, `conversations.history`,
//!   `conversations.replies` — all cursor-paginated; synthesize the
//!   cursor chain so the live extract walks it the same way.
//!
//! Slack URLs are POST-shaped in production (`x-www-form-urlencoded`
//! body) but we issue them as GETs with query params; match the
//! `HttpRequest::get("slack", ...)` shape used by extract.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct SlackSynth {
    pub api_dir: PathBuf,
}

impl SlackSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for SlackSynth {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
