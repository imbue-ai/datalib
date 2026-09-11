//! Provider-owned config schema for the `slack` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// The live Slack API — the one way in.
    #[serde(default)]
    pub api: Option<SlackApiSync>,
}

impl SlackConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            api.validate()?;
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
    #[serde(default)]
    pub dms: bool,
    /// Restrict DM mirroring to these conversations, by Slack's own id
    /// for each (`D…` for a 1:1, `G…` or `C…` for a group DM) or a
    /// pasted link to it — the `Copy link` on a DM gives
    /// `https://<workspace>.slack.com/archives/<id>`. Unset (with
    /// `dms = true`) means every DM the account can see.
    #[serde(default)]
    pub dm_conversations: Option<Vec<String>>,
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
            dm_conversations: None,
        }
    }
}

impl SlackApiSync {
    /// `dm_conversations` without `dms = true` is rejected rather than
    /// silently resolved either way. Both silent readings are bad:
    /// honoring the list would start mirroring DMs from a config that
    /// never asked to, and ignoring it would mirror nothing while the
    /// file plainly says which conversations to mirror. Neither is
    /// discoverable from the outcome, so this fails at config-load
    /// time with the fix in the message.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.dms {
            if let Some(convs) = &self.dm_conversations {
                if !convs.is_empty() {
                    anyhow::bail!(
                        "`dm_conversations` lists {} entr{} but `dms` is false, so no direct \
                         messages would be mirrored at all. Set `dms = true` to mirror \
                         those conversations, or drop `dm_conversations` to turn DMs off.",
                        convs.len(),
                        if convs.len() == 1 { "y" } else { "ies" },
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

impl datalib_source_common::IngestMethods for SlackConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync(dms: bool, dm_conversations: Option<Vec<&str>>) -> SlackApiSync {
        SlackApiSync {
            dms,
            dm_conversations: dm_conversations.map(|v| v.into_iter().map(String::from).collect()),
            ..Default::default()
        }
    }

    /// The backward-compatible shape: a config written before this
    /// field existed leaves DMs off.
    #[test]
    fn dms_default_off() {
        assert!(!SlackApiSync::default().dms);
        assert!(SlackApiSync::default().dm_conversations.is_none());
        SlackApiSync::default().validate().unwrap();
    }

    #[test]
    fn dm_conversations_without_dms_is_rejected() {
        let err = sync(false, Some(vec!["D0123ABCD"]))
            .validate()
            .expect_err("should reject");
        let msg = err.to_string();
        // The message has to name the fix, since neither silent reading
        // of this combination is discoverable from the outcome.
        assert!(msg.contains("dm_conversations"), "{msg}");
        assert!(msg.contains("dms = true"), "{msg}");
    }

    #[test]
    fn dm_conversations_with_dms_is_accepted() {
        sync(true, Some(vec!["D0123ABCD"])).validate().unwrap();
    }

    /// An empty list is the same as none — it asks for nothing, so it
    /// can't be the "you forgot the switch" mistake the error catches.
    #[test]
    fn empty_dm_conversations_without_dms_is_fine() {
        sync(false, Some(vec![])).validate().unwrap();
    }

    /// The people-shaped field this replaced. `deny_unknown_fields`
    /// already refuses it; this pins that a stale config fails at load
    /// rather than quietly mirroring every DM.
    #[test]
    fn the_old_dm_users_field_is_refused() {
        let err = toml::from_str::<SlackConfig>("[api]\ndms = true\ndm_users = [\"@riker\"]\n")
            .expect_err("dm_users is gone")
            .to_string();
        assert!(err.contains("dm_users"), "{err}");
    }

    /// `validate()` on the whole config has to reach the `api` table,
    /// which is the wiring the step dispatcher actually calls.
    #[test]
    fn config_validate_reaches_api() {
        let cfg = SlackConfig {
            api: Some(sync(false, Some(vec!["D0123ABCD"]))),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
        // No `api` table: nothing for the schema to check. Refusing an
        // ingest step without one is `datalib-step`'s job.
        SlackConfig::default().validate().unwrap();
    }
}
