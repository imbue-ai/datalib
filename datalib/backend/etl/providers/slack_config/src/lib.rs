//! Provider-owned config schema for the `slack_api` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. Composed only by the
    /// providers that authenticate through the `latchkey` CLI, and
    /// forwarded whole to the download client — see [`LatchkeySettings`].
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    #[serde(default)]
    pub sync: Option<SlackApiSync>,
}

impl SlackConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(sync) = &self.sync {
            sync.validate()?;
        }
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
    /// Mirror direct messages — both 1:1 DMs and group DMs — alongside
    /// channels. **Off unless set**, and deliberately so: DMs are the
    /// most sensitive thing in a workspace, and an upgrade must not
    /// start mirroring them because a new field appeared.
    ///
    /// Orthogonal to `channels` / `all_channels`, which scope the
    /// channel half only. Setting `channels` and `dms = true` mirrors
    /// those channels *and* your DMs; it does not filter DMs by
    /// channel name (a DM has no channel name to match).
    #[serde(default)]
    pub dms: bool,
    /// Restrict DM mirroring to conversations with these people. Unset
    /// (with `dms = true`) means every DM the account can see.
    ///
    /// Entries name a *person*, not a conversation, because that is the
    /// only handle a DM has: a Slack user id (`U024BE7LH`) or any of
    /// that user's names — handle, display name, or real name — with an
    /// optional leading `@`. Matching is case-insensitive. This is the
    /// `@`-namespace counterpart to `channels`' `#`-namespace, which is
    /// why it is a separate list rather than more entries in `channels`.
    ///
    /// An entry that matches nobody in the mirrored user directory is a
    /// loud `warn!`, not a silent empty result.
    ///
    /// **Group DMs are skipped while this is set.** `conversations.list`
    /// describes an `mpim` with a mangled composite handle
    /// (`mpdm-alice--bob--carol-1`) and no member list, so "is this
    /// group a conversation with Alice?" can't be answered without a
    /// per-group extra call. Rather than guess by string-splitting a
    /// name that may itself contain dashes, the allowlist covers 1:1
    /// DMs only; leave it unset to mirror group DMs too.
    ///
    /// Requires `dms = true` — see [`SlackApiSync::validate`].
    #[serde(default)]
    pub dm_users: Option<Vec<String>>,
}

impl Default for SlackApiSync {
    fn default() -> Self {
        Self {
            refresh_window_days: None,
            channels: None,
            since: None,
            all_channels: false,
            media: true,
            dms: false,
            dm_users: None,
        }
    }
}

impl SlackApiSync {
    /// `dm_users` without `dms = true` is rejected rather than silently
    /// resolved either way. Both silent readings are bad: honoring the
    /// list would start mirroring DMs from a config that never asked
    /// to, and ignoring it would mirror nothing while the file plainly
    /// says which people to mirror. Neither is discoverable from the
    /// outcome, so this fails at config-load time with the fix in the
    /// message.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.dms {
            if let Some(users) = &self.dm_users {
                if !users.is_empty() {
                    anyhow::bail!(
                        "`dm_users` lists {} entr{} but `dms` is false, so no direct \
                         messages would be mirrored at all. Set `dms = true` to mirror \
                         DMs with those people, or drop `dm_users` to turn DMs off.",
                        users.len(),
                        if users.len() == 1 { "y" } else { "ies" },
                    );
                }
            }
        }
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type SlackRenderConfig = datalib_source_common::BareRenderConfig;

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(dms: bool, dm_users: Option<Vec<&str>>) -> SlackApiSync {
        SlackApiSync {
            dms,
            dm_users: dm_users.map(|v| v.into_iter().map(String::from).collect()),
            ..Default::default()
        }
    }

    /// The backward-compatible shape: a config written before this
    /// field existed leaves DMs off.
    #[test]
    fn dms_default_off() {
        assert!(!SlackApiSync::default().dms);
        assert!(SlackApiSync::default().dm_users.is_none());
        SlackApiSync::default().validate().unwrap();
    }

    #[test]
    fn dm_users_without_dms_is_rejected() {
        let err = sync(false, Some(vec!["alice"]))
            .validate()
            .expect_err("should reject");
        let msg = err.to_string();
        // The message has to name the fix, since neither silent reading
        // of this combination is discoverable from the outcome.
        assert!(msg.contains("dm_users"), "{msg}");
        assert!(msg.contains("dms = true"), "{msg}");
    }

    #[test]
    fn dm_users_with_dms_is_accepted() {
        sync(true, Some(vec!["alice"])).validate().unwrap();
    }

    /// An empty list is the same as none — it asks for nothing, so it
    /// can't be the "you forgot the switch" mistake the error catches.
    #[test]
    fn empty_dm_users_without_dms_is_fine() {
        sync(false, Some(vec![])).validate().unwrap();
    }

    /// `validate()` on the whole config has to reach the `sync` table,
    /// which is the wiring the step dispatcher actually calls.
    #[test]
    fn config_validate_reaches_sync() {
        let cfg = SlackConfig {
            sync: Some(sync(false, Some(vec!["alice"]))),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        // No `sync` table at all = unmanaged, no download wave;
        // nothing to check.
        SlackConfig::default().validate().unwrap();
    }
}
