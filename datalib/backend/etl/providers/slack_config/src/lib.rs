//! Provider-owned config schema for the `slack_api` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<SlackApiSync>,
}

impl SlackConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackApiSync {
    /// Trailing edit-catcher — *not* a way to say "only fetch the last N
    /// days". On a channel that already has history, re-query the last N
    /// days on top of the forward walk so edits and reactions on
    /// already-stored messages land. It only ever *adds* work: the range
    /// a run fetches still starts at `since`. Unset (or `0`) skips the
    /// pass. Note the CLI's `--refresh-window-days` defaults to 30
    /// instead; a config-driven run gets 0 unless you set it here.
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    /// Channel names to mirror, without the `#`. Unset means every
    /// channel the account is a member of (or every channel it can see,
    /// with `all_channels`).
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    /// Oldest message to fetch — `YYYY-MM-DD` or RFC 3339. This is the
    /// knob that decides how far back the mirror goes, so "just the last
    /// week" means setting this to seven days ago. Unset defaults to
    /// 2024-01-01 (the provider's `DEFAULT_SINCE`).
    ///
    /// Moving it earlier backfills the newly-covered window on the next
    /// run; moving it later is a no-op, since nothing in the pipeline
    /// deletes already-mirrored messages. See the provider's
    /// `DOWNLOAD.md` for how that is detected.
    #[serde(default)]
    pub since: Option<String>,
    /// Also mirror channels the account can see but isn't a member of.
    /// Ignored when `channels` is set.
    #[serde(default)]
    pub all_channels: bool,
    /// Download file attachments into blobs. Off = JSON metadata only.
    #[serde(default = "default_true")]
    pub media: bool,
}

impl Default for SlackApiSync {
    fn default() -> Self {
        Self {
            refresh_window_days: None,
            channels: None,
            since: None,
            all_channels: false,
            media: true,
        }
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type SlackRenderConfig = datalib_source_common::BareRenderConfig;
