//! Stub: Notion HTTP fixture synthesizer.
//!
//! TODO: walk event-store JSONL under `<api_dir>/notion_official_{page,
//! block,comment}/{created,updated}/events.jsonl` and emit fixtures for
//! every call [`crate::extract::official`] would issue:
//!
//! * `GET https://api.notion.com/v1/pages/{id}` — one per page id
//! * `GET https://api.notion.com/v1/blocks/{id}/children?...` — paginated
//!   walks under each page (need to reconstruct the cursor chain so
//!   subsequent extracts see `has_more`/`next_cursor` correctly).
//! * `GET https://api.notion.com/v1/comments?block_id={id}&...` — same
//!   pagination caveat.
//! * `POST https://api.notion.com/v1/databases/{id}/query` — database
//!   row enumeration.
//!
//! The unofficial branch (`notion_unofficial`) is out of scope for
//! playback today; it only runs against the inbox and we'll drive that
//! via a separate synthesizer if/when needed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct NotionSynth {
    pub api_dir: PathBuf,
}

impl NotionSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for NotionSynth {
    fn name(&self) -> &'static str {
        "notion"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
