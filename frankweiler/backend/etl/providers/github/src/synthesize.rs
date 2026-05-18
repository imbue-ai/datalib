//! Stub: GitHub HTTP fixture synthesizer.
//!
//! TODO: walk the `github_api/` event-store and emit fixtures for every
//! call [`crate::extract`] would issue:
//!
//! * `GET https://api.github.com/user` — viewer identity.
//! * `GET https://api.github.com/search/issues?q=...` — paginated PR
//!   discovery; `Link: rel="next"` drives the loop, so synthesized
//!   fixtures must include the matching `link` header (last page
//!   omits the `next` rel).
//! * Per-PR detail expansion:
//!   * `GET /repos/{owner}/{repo}/pulls/{num}`
//!   * `GET /repos/{owner}/{repo}/issues/{num}/comments`
//!   * `GET /repos/{owner}/{repo}/pulls/{num}/reviews`
//!   * `GET /repos/{owner}/{repo}/pulls/{num}/comments`
//!
//! Each PR's event-store record holds the `raw` JSON; we can lift it
//! directly into the fixture body for the single-resource endpoints and
//! synthesize the search-results envelope around it for the discovery
//! pages.

use std::path::{Path, PathBuf};

use anyhow::Result;
use frankweiler_etl::synthesize::{SynthesizeReport, Synthesizer};

pub struct GithubSynth {
    pub api_dir: PathBuf,
}

impl GithubSynth {
    pub fn new(api_dir: impl Into<PathBuf>) -> Self {
        Self {
            api_dir: api_dir.into(),
        }
    }
}

impl Synthesizer for GithubSynth {
    fn name(&self) -> &'static str {
        "github"
    }

    fn synthesize(&self, _out_root: &Path) -> Result<SynthesizeReport> {
        let _ = &self.api_dir;
        Ok(SynthesizeReport::default())
    }
}
