//! Stub: GitLab HTTP fixture synthesizer.
//!
//! TODO: walk `gitlab_api/` event-store and emit fixtures for every
//! call [`crate::extract`] would issue:
//!
//! * `GET /api/v4/user` — viewer identity.
//! * `GET /api/v4/merge_requests?...` — paginated MR discovery (keyset
//!   pagination via `id_after` / `Link` header).
//! * Per-MR detail: `/projects/{id}/merge_requests/{iid}` plus
//!   `/notes`, `/discussions`, `/approvals`.
//!
//! Reconstruct pagination from event-store records and either include
//! the GitLab `Link` header in the synthesized response or surface the
//! cursor in the body — match whichever shape the live client reads.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct GitlabSynth {
    pub api_dir: PathBuf,
}

impl GitlabSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for GitlabSynth {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
