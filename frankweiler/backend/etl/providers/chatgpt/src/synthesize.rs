//! Stub: ChatGPT HTTP fixture synthesizer.
//!
//! TODO: walk `chatgpt_api/` event store and emit fixtures for the two
//! endpoints [`crate::extract`] calls:
//!
//! * `GET https://chatgpt.com/backend-api/me` — viewer identity.
//! * `GET https://chatgpt.com/backend-api/conversations?offset=N&limit=M`
//!   — offset-paginated listing.
//! * `GET https://chatgpt.com/backend-api/conversation/{id}` — full
//!   conversation payload, one fixture per conversation id.
//!
//! Offset pagination is straightforward — emit one fixture per
//! `(offset, limit)` pair the live extract would request, terminating
//! the chain with an empty `items` list.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct ChatgptSynth {
    pub api_dir: PathBuf,
}

impl ChatgptSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for ChatgptSynth {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
