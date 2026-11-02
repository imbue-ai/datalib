//! Provider-owned config schema for the `slack` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use datalib_source_common::{LatchkeySettings, SourceCommon};
use datalib_time::TimeBound;
use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "api",
        toml: include_str!("../examples/minimal_api.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "api",
        toml: include_str!("../examples/complete_api.toml"),
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SlackApiSync {
    /// Trailing edit-catcher — *not* a way to say "only fetch the last N
    /// days". On a channel that already has history, re-query the last N
    /// days on top of the forward walk so edits and reactions on
    /// already-stored messages land, and messages deleted upstream go.
    /// It only ever *adds* work: the range a run fetches still starts at
    /// `since`. Unset is 30 days; `0` skips the pass.
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    /// How many minutes after a conversation's refresh window was last
    /// re-read before a run re-reads it again, so a source synced often
    /// does not re-walk every channel each time. Unset is 60; `0`
    /// re-reads it on every run.
    #[serde(default)]
    pub refresh_every_minutes: Option<i64>,
    /// Channel names to mirror, without the `#`. Unset means every
    /// channel the account is a member of (or every channel it can see,
    /// with `all_channels`).
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    /// Oldest message to fetch: a day (`2026-09-01`), an RFC 3339
    /// instant, or an offset from the run's start (`-30d`, `-2w`). This
    /// is the knob that decides how far back the mirror goes. An offset
    /// slides forward each run; a message it leaves behind stays in the
    /// mirror. Unset defaults to 2024-01-01 (the provider's
    /// `DEFAULT_SINCE`). A date before 1970 asks for everything.
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
    /// `dms = true`) means every DM the account can see. Either way, a DM
    /// is read once, then while it is open in Slack's sidebar with
    /// something new, and once a day while it is closed.
    #[serde(default)]
    pub dm_conversations: Option<Vec<String>>,
    /// Which channels a sync walks. `every_conversation` (the default)
    /// walks every channel `channels` / `all_channels` choose.
    /// `involves_me` walks only the channels where something involves
    /// the account, each from a little before the oldest such message;
    /// DMs follow `dms` either way.
    #[serde(default)]
    pub scope: Scope,
    /// With `scope = "involves_me"`: what counts as involving the
    /// account. Unset is every kind.
    #[serde(default)]
    pub involves_me: Option<Vec<InvolvesMe>>,
    /// With `scope = "involves_me"`: how long before the oldest message
    /// involving the account a channel is walked from, as an offset
    /// (`"-1d"`, `"-3h"`). Unset is `-1d`.
    #[serde(default)]
    pub context_before: Option<String>,
    /// With `scope = "involves_me"`: how long after each message
    /// involving the account to walk, as an offset (`"+1d"`). Unset
    /// walks a channel up to now.
    #[serde(default)]
    pub context_after: Option<String>,
}

/// Which channels a sync walks.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Scope {
    #[default]
    EveryConversation,
    InvolvesMe,
}

/// A kind of message that involves the account, each found by a Slack
/// search.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum InvolvesMe {
    /// A message that mentions the account by name.
    Mention,
    /// A message the account posted.
    FromMe,
    /// A message that says `@here` or `@channel`.
    Broadcast,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

impl InvolvesMe {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// The default of `context_before`.
pub const DEFAULT_CONTEXT_BEFORE: &str = "-1d";

/// An offset from a message, as `context_before` or `context_after`
/// write it: the offset form of the shared window grammar, signed
/// `sign`. A day or an instant names no distance from a message.
fn context_offset(field: &str, s: &str, sign: i32) -> anyhow::Result<chrono::Duration> {
    let expected = if sign < 0 { "-1d" } else { "+1d" };
    match TimeBound::parse(s) {
        Ok(TimeBound::Relative(d))
            if (sign < 0 && d <= chrono::Duration::zero())
                || (sign > 0 && d >= chrono::Duration::zero()) =>
        {
            Ok(d)
        }
        _ => anyhow::bail!(
            "`api.{field}` is {s:?}: write an offset from each message, like {expected:?} \
             (units h, d, w)"
        ),
    }
}

impl Default for SlackApiSync {
    fn default() -> Self {
        Self {
            refresh_window_days: None,
            refresh_every_minutes: None,
            channels: None,
            since: None,
            all_channels: false,
            media: true,
            dms: false,
            dm_conversations: None,
            scope: Scope::default(),
            involves_me: None,
            context_before: None,
            context_after: None,
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
        datalib_time::TimeWindow::check("api", self.since.as_deref(), None)
            .map_err(anyhow::Error::msg)?;
        datalib_source_common::not_negative(
            "api.refresh_every_minutes",
            self.refresh_every_minutes,
        )
        .map_err(anyhow::Error::msg)?;
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
        self.context_before()?;
        self.context_after()?;
        if self.scope != Scope::InvolvesMe {
            let set: Vec<&str> = [
                ("involves_me", self.involves_me.is_some()),
                ("context_before", self.context_before.is_some()),
                ("context_after", self.context_after.is_some()),
            ]
            .into_iter()
            .filter_map(|(name, set)| set.then_some(name))
            .collect();
            if !set.is_empty() {
                anyhow::bail!(
                    "`{}` only applies with `scope = \"involves_me\"`, and `scope` is {:?}. \
                     Set the scope, or drop {}.",
                    set.join("`, `"),
                    self.scope.as_str(),
                    if set.len() == 1 { "it" } else { "them" },
                );
            }
        }
        if self.involves_me.as_ref().is_some_and(Vec::is_empty) {
            anyhow::bail!(
                "`api.involves_me` is empty, so nothing would count as involving you and no \
                 channel would be walked. List at least one of {:?}, or drop it for all of them.",
                InvolvesMe::VARIANTS
                    .iter()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// The kinds of message that count as involving the account, each
    /// once, in a fixed order.
    pub fn involves_me_kinds(&self) -> Vec<InvolvesMe> {
        let mut kinds = self
            .involves_me
            .clone()
            .unwrap_or_else(|| InvolvesMe::VARIANTS.to_vec());
        kinds.sort();
        kinds.dedup();
        kinds
    }

    /// `context_before` as a duration, at most zero.
    pub fn context_before(&self) -> anyhow::Result<chrono::Duration> {
        context_offset(
            "context_before",
            self.context_before
                .as_deref()
                .unwrap_or(DEFAULT_CONTEXT_BEFORE),
            -1,
        )
    }

    /// `context_after` as a duration, at least zero; `None` is up to now.
    pub fn context_after(&self) -> anyhow::Result<Option<chrono::Duration>> {
        self.context_after
            .as_deref()
            .map(|s| context_offset("context_after", s, 1))
            .transpose()
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

    /// `since` takes the shared window grammar: what loaded before
    /// still loads, an offset loads, and a typo is refused at load.
    #[test]
    fn since_takes_the_window_grammar() {
        let since = |s: &str| {
            SlackApiSync {
                since: Some(s.into()),
                ..Default::default()
            }
            .validate()
        };
        for ok in [
            "2024-01-01",
            "2024-03-20T18:28:51Z",
            "1900-01-01",
            "-30d",
            "-2w",
        ] {
            since(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let err = since("30 days").unwrap_err().to_string();
        assert!(err.contains("`api.since`"), "{err}");
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

    fn involving(f: impl FnOnce(&mut SlackApiSync)) -> anyhow::Result<()> {
        let mut sync = SlackApiSync {
            scope: Scope::InvolvesMe,
            ..Default::default()
        };
        f(&mut sync);
        sync.validate()
    }

    #[test]
    fn strum_and_serde_agree() {
        for scope in Scope::VARIANTS {
            let json = serde_json::to_value(scope).unwrap();
            assert_eq!(json, scope.as_str());
            assert_eq!(Scope::parse(scope.as_str()), Some(*scope));
        }
        for kind in InvolvesMe::VARIANTS {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, kind.as_str());
            assert_eq!(InvolvesMe::parse(kind.as_str()), Some(*kind));
        }
    }

    /// The defaults are the wide ones: every kind, a day before, and up
    /// to now after.
    #[test]
    fn involves_me_defaults_are_wide() {
        let sync = SlackApiSync {
            scope: Scope::InvolvesMe,
            ..Default::default()
        };
        sync.validate().unwrap();
        assert_eq!(sync.involves_me_kinds(), InvolvesMe::VARIANTS);
        assert_eq!(sync.context_before().unwrap(), chrono::Duration::days(-1));
        assert_eq!(sync.context_after().unwrap(), None);
    }

    #[test]
    fn a_toml_scope_loads() {
        let cfg: SlackConfig = toml::from_str(
            "[api]\nscope = \"involves_me\"\ninvolves_me = [\"mention\", \"from_me\"]\n\
             context_before = \"-3h\"\ncontext_after = \"+2d\"\n",
        )
        .unwrap();
        cfg.validate().unwrap();
        let api = cfg.api.unwrap();
        assert_eq!(
            api.involves_me_kinds(),
            [InvolvesMe::Mention, InvolvesMe::FromMe]
        );
        assert_eq!(api.context_before().unwrap(), chrono::Duration::hours(-3));
        assert_eq!(
            api.context_after().unwrap(),
            Some(chrono::Duration::days(2))
        );
    }

    /// The context settings take the offset form of the window grammar,
    /// pointing the right way; a day or an instant is no distance from a
    /// message.
    #[test]
    fn context_takes_offsets_that_point_the_right_way() {
        for bad in ["2026-09-01", "2026-09-01T09:00:00Z", "+1d", "1 day"] {
            let err = involving(|s| s.context_before = Some(bad.into()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("`api.context_before`"), "{bad}: {err}");
        }
        for bad in ["2026-09-01", "-1d"] {
            let err = involving(|s| s.context_after = Some(bad.into()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("`api.context_after`"), "{bad}: {err}");
        }
        involving(|s| s.context_before = Some("-2w".into())).unwrap();
        involving(|s| s.context_after = Some("+12h".into())).unwrap();
    }

    /// A setting of the narrow scope on the default one would do
    /// nothing, silently.
    #[test]
    fn involves_me_settings_need_the_scope() {
        let err = SlackApiSync {
            context_before: Some("-3h".into()),
            ..Default::default()
        }
        .validate()
        .unwrap_err()
        .to_string();
        assert!(err.contains("scope = \"involves_me\""), "{err}");
    }

    #[test]
    fn an_empty_involves_me_is_refused() {
        let err = involving(|s| s.involves_me = Some(vec![]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`api.involves_me` is empty"), "{err}");
    }
}
