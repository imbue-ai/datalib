//! Slack render stage: raw → typed buckets ready for render.

pub mod ids;
pub mod mrkdwn;
pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

use serde_json::Value;

pub use parse::{parse, ParsedSlack, ScanResult, SlackThreadBucket};

pub use datalib_etl_slack::ingest::schema_raw::{slack_message_key, slack_thread_key};
pub use ids::{parse_slack_ts, ts_to_iso, ts_to_ms};

#[derive(Debug, Clone)]
pub struct User {
    pub user_id: String,
    pub team_id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
    /// `profile.email`. Slack only serves it with the `users:read.email`
    /// scope, so it is often absent for everyone but the account itself.
    pub email: Option<String>,
}

impl User {
    pub fn label(&self) -> String {
        datalib_etl_slack::user_label(
            self.real_name.as_deref(),
            self.name.as_deref(),
            &self.user_id,
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub channel_id: String,
    /// `general` for a channel, an `mpdm-…` composite handle for a
    /// group DM, `None` for a 1:1 DM — Slack gives one no name.
    pub name: Option<String>,
    /// A direct message surface (`is_im` or `is_mpim`).
    pub is_dm: bool,
    /// Who is in this DM, as Slack listed them — self included for a
    /// group DM. Empty for a channel. See
    /// [`datalib_etl_slack::ingest::schema_raw::ChannelRow::dm_user_ids`].
    pub dm_user_ids: Vec<String>,
}

impl Channel {
    pub fn display(
        &self,
        users: datalib_etl_render::inputs::Lookup<'_, std::collections::BTreeMap<String, String>>,
        self_user_id: Option<&str>,
    ) -> String {
        if !self.is_dm {
            return format!(
                "#{}",
                self.name.clone().unwrap_or_else(|| self.channel_id.clone())
            );
        }
        let counterparts =
            datalib_etl_slack::ingest::schema_raw::dm_counterparts(&self.dm_user_ids, self_user_id);
        let labels: std::collections::BTreeMap<String, String> = counterparts
            .iter()
            .filter_map(|u| Some((u.clone(), users.get(u)?.clone())))
            .collect();
        datalib_etl_slack::ingest::schema_raw::dm_display_name(
            &counterparts,
            self.name.as_deref(),
            &self.channel_id,
            &labels,
        )
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    /// The `workspaces` row's primary key, which is what a thread
    /// declares it read.
    pub row_id: String,
    pub team_id: String,
    pub team_name: Option<String>,
    pub team_url: Option<String>,
    pub self_user_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub effective_thread_ts: String,
    pub is_thread_root: bool,
    pub user_id: Option<String>,
    pub text: String,
    /// The parsed `ts` as ISO-8601, or `None` when `ts` isn't a shape
    /// we can read. Used as the primary sort key; `None` sorts first,
    /// which is where an unreadable `ts` used to land anyway when it
    /// was silently read as the epoch.
    pub ts_iso: Option<String>,
    /// Original Slack message JSON, preserved verbatim. The renderer
    /// reaches into this for `files`, `reactions`, and any future
    /// field we don't promote to a struct member.
    pub raw_json: Value,
}

impl Message {
    /// The raw store's key for this message.
    pub fn key(&self) -> String {
        slack_message_key(&self.team_id, &self.channel_id, &self.ts)
    }
    /// The raw store's key for this message's thread: the bucket key.
    pub fn thread_key(&self) -> String {
        slack_thread_key(&self.team_id, &self.channel_id, &self.effective_thread_ts)
    }
}

pub use mrkdwn::{resolve_mentions, Labels};

/// A Slack message permalink. With `thread_ts` (and when it differs from
/// `ts`) the reply-in-thread params are appended so the link deep-links
/// to the threaded message rather than the channel root.
pub fn slack_link(team_id: &str, channel_id: &str, ts: &str, thread_ts: Option<&str>) -> String {
    let ts_no_dot: String = ts.chars().filter(|c| *c != '.').collect();
    let mut url = format!("https://slack.com/archives/{channel_id}/p{ts_no_dot}?team={team_id}");
    if let Some(tts) = thread_ts {
        if tts != ts {
            url.push_str(&format!("&thread_ts={tts}&cid={channel_id}"));
        }
    }
    url
}

#[cfg(test)]
mod channel_display_tests {
    use super::*;
    use datalib_etl_render::inputs::Inputs;
    use std::collections::BTreeMap;

    /// U1 is the account doing the mirroring.
    const SELF: Option<&str> = Some("U1");

    fn users() -> BTreeMap<String, String> {
        [
            ("U1", "Jean-Luc Picard"),
            ("U2", "William Riker"),
            ("U3", "Data"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn dm(id: &str, name: Option<&str>, participants: &[&str]) -> Channel {
        Channel {
            channel_id: id.into(),
            name: name.map(String::from),
            is_dm: true,
            dm_user_ids: participants.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Channels render exactly as they always have — including the
    /// id fallback. This is what keeps the render goldens byte-stable
    /// across the DM change.
    #[test]
    fn a_channel_is_hash_name() {
        let inputs = Inputs::default();
        let users = users();
        let c = Channel {
            channel_id: "C1".into(),
            name: Some("general".into()),
            ..Default::default()
        };
        assert_eq!(c.display(inputs.lookup("users", &users), SELF), "#general");

        let unnamed = Channel {
            channel_id: "C2".into(),
            ..Default::default()
        };
        assert_eq!(unnamed.display(inputs.lookup("users", &users), SELF), "#C2");
    }

    /// The reason DMs need their own branch: a 1:1 DM has no name, so
    /// the channel path would title every one of them `#D0123ABCD`.
    #[test]
    fn a_dm_is_at_the_person() {
        let inputs = Inputs::default();
        let users = users();
        assert_eq!(
            dm("D1", None, &["U2"]).display(inputs.lookup("users", &users), SELF),
            "@William Riker"
        );
    }

    /// A group DM's `members` includes the account itself. Titling it
    /// with your own name in the list is not what anyone means by "who
    /// is this conversation with".
    #[test]
    fn a_group_dm_names_the_others() {
        let inputs = Inputs::default();
        let users = users();
        assert_eq!(
            dm(
                "G1",
                Some("mpdm-picard--riker--data-1"),
                &["U1", "U2", "U3"]
            )
            .display(inputs.lookup("users", &users), SELF),
            "@William Riker, Data"
        );
    }

    #[test]
    fn a_dm_with_an_unknown_user_falls_back_to_the_user_id() {
        let inputs = Inputs::default();
        let users = users();
        assert_eq!(
            dm("D9", None, &["U404"]).display(inputs.lookup("users", &users), SELF),
            "@U404"
        );
    }

    /// A store written before `dm_user_ids` existed, or a shape without
    /// participants: Slack's own composite handle, then the raw id.
    #[test]
    fn a_dm_without_participants_falls_back_to_the_handle_then_the_id() {
        let inputs = Inputs::default();
        let users = users();
        assert_eq!(
            dm("G1", Some("mpdm-picard--riker--data-1"), &[])
                .display(inputs.lookup("users", &users), SELF),
            "@mpdm-picard--riker--data-1"
        );
        assert_eq!(
            dm("D9", None, &[]).display(inputs.lookup("users", &users), SELF),
            "D9"
        );
    }

    /// A DM with yourself still has to be nameable.
    #[test]
    fn a_note_to_self_keeps_your_own_name() {
        let inputs = Inputs::default();
        let users = users();
        assert_eq!(
            dm("D0", None, &["U1"]).display(inputs.lookup("users", &users), SELF),
            "@Jean-Luc Picard"
        );
    }
}
