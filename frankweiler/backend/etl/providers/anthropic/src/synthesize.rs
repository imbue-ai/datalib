//! Stub: Anthropic (claude.ai) HTTP fixture synthesizer.
//!
//! TODO: walk `anthropic_api/` event store and emit fixtures for the
//! endpoints [`crate::extract`] calls:
//!
//! * `GET https://claude.ai/api/organizations` — org discovery.
//! * `GET https://claude.ai/api/organizations/{org}/chat_conversations`
//!   — paginated listing.
//! * `GET https://claude.ai/api/organizations/{org}/chat_conversations/{id}`
//!   — full conversation, one fixture per id.
//!
//! Lift the `raw` field from each event-store record straight into the
//! detail-endpoint fixture body; synthesize the listing envelope around
//! the ids.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct AnthropicSynth {
    pub api_dir: PathBuf,
}

impl AnthropicSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for AnthropicSynth {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
