//! Schema-only config crate for the `linkedin` source (Program A goal #1).
//!
//! A LinkedIn export is a file-backed "takeout": [`LinkedinConfig`] carries
//! the one source-specific knob — `fetch_photos` — at the top level (there is
//! no `sync:` block). The provider's `plan()` consumes this alongside the
//! envelope-level `PlanCommon`. Bazel-only by design (no Cargo.toml).

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Typed config for a `linkedin` source. `fetch_photos` lives at the top
/// level (LinkedIn has no managed `sync:` block).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkedinConfig {
    /// Whether to download connection profile photos during extract.
    #[serde(default)]
    pub fetch_photos: bool,
}

impl LinkedinConfig {
    /// No cross-field constraints to check.
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}
