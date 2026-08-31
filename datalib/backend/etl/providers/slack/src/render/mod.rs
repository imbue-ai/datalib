//! Slack render stage: raw → typed buckets ready for render.
//!
//! Entry point is [`parse::parse`]: it opens the doltlite DB, runs
//! the `dolt_diff_<table>` scan against the render cursor, and
//! loads only the changed thread buckets — each one carrying its
//! own per-thread [`datalib_etl::blob_cas::BlobBundle`] so render
//! is fully sync. Falls back to the legacy JSON-tree reader for the
//! in-crate fixture (cold-start only, every thread rendered).
//!
//! Determinism: row UUIDs are `uuid::Uuid::new_v5` with the slack
//! namespace defined in `download::schema_raw`. Same hash for the same
//! source data across re-ingest.

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

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

// UUIDv5 recipes for Slack message and thread ids live in
// `download::schema_raw`. Re-export here so existing
// `crate::render::slack_message_uuid` callers outside this crate
// keep resolving.
pub use super::download::schema_raw::{slack_message_uuid, slack_thread_uuid};
pub use parse::{parse, ParsedSlack, ScanResult, SlackThreadBucket};

/// Render Slack `ts` (unix seconds + fractional, UTC) as ISO-8601
/// with microsecond precision and `+00:00` offset.
pub fn ts_to_iso(ts: &str) -> String {
    let (secs_str, frac_str) = ts.split_once('.').unwrap_or((ts, ""));
    let secs: i64 = secs_str.parse().unwrap_or(0);
    let mut frac = frac_str.to_string();
    if frac.len() < 6 {
        frac.push_str(&"0".repeat(6 - frac.len()));
    } else if frac.len() > 6 {
        frac.truncate(6);
    }
    let micros: u32 = frac.parse().unwrap_or(0);
    let dt: DateTime<Utc> = Utc
        .timestamp_opt(secs, micros * 1_000)
        .single()
        .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap());
    dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

#[derive(Debug, Clone)]
pub struct User {
    pub user_id: String,
    pub team_id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
}

impl User {
    pub fn label(&self) -> String {
        crate::user_label(
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
    /// [`crate::download::schema_raw::ChannelRow::dm_user_ids`].
    pub dm_user_ids: Vec<String>,
}

impl Channel {
    /// How this conversation is titled in rendered markdown and in the
    /// grid's `conversation_name`.
    ///
    /// A channel keeps the `#name` it has always had — including the
    /// `#<channel_id>` fallback for a channel whose name we never
    /// captured, which is why the fallback lives here rather than at
    /// the call site.
    ///
    /// A DM is named after the people in it, `@`-sigilled: a 1:1 DM has
    /// no name to put after a `#`, and `#D0123ABCD` is not something
    /// anyone can read. The account itself is subtracted, so a DM reads
    /// as who you are talking *to*.
    ///
    /// `users` maps user id → display label ([`User::label`]).
    pub fn display(
        &self,
        users: &std::collections::BTreeMap<String, String>,
        self_user_id: Option<&str>,
    ) -> String {
        if !self.is_dm {
            return format!(
                "#{}",
                self.name.clone().unwrap_or_else(|| self.channel_id.clone())
            );
        }
        let counterparts =
            crate::download::schema_raw::dm_counterparts(&self.dm_user_ids, self_user_id);
        if !counterparts.is_empty() {
            let names: Vec<String> = counterparts
                .iter()
                .map(|u| users.get(u).cloned().unwrap_or_else(|| u.clone()))
                .collect();
            return format!("@{}", names.join(", "));
        }
        // Nothing resolvable: fall back to Slack's own composite
        // handle (`mpdm-alice--bob--carol-1`), then the raw id. Ugly
        // but truthful — splitting that handle back into people is
        // guesswork, since a Slack handle may itself contain dashes.
        match &self.name {
            Some(n) => format!("@{n}"),
            None => self.channel_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
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
    pub ts_iso: String,
    /// Original Slack message JSON, preserved verbatim. The renderer
    /// reaches into this for `files`, `reactions`, and any future
    /// field we don't promote to a struct member.
    pub raw_json: Value,
}

impl Message {
    pub fn uuid(&self) -> String {
        slack_message_uuid(&self.team_id, &self.channel_id, &self.ts)
    }
    pub fn thread_uuid(&self) -> String {
        slack_thread_uuid(&self.team_id, &self.channel_id, &self.effective_thread_ts)
    }
}

pub use mrkdwn::resolve_user_mentions;

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
    use std::collections::BTreeMap;

    /// U1 is the account doing the mirroring.
    const SELF: Option<&str> = Some("U1");

    fn labels() -> BTreeMap<String, String> {
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
        let c = Channel {
            channel_id: "C1".into(),
            name: Some("general".into()),
            ..Default::default()
        };
        assert_eq!(c.display(&labels(), SELF), "#general");

        let unnamed = Channel {
            channel_id: "C2".into(),
            ..Default::default()
        };
        assert_eq!(unnamed.display(&labels(), SELF), "#C2");
    }

    /// The reason DMs need their own branch: a 1:1 DM has no name, so
    /// the channel path would title every one of them `#D0123ABCD`.
    #[test]
    fn a_dm_is_at_the_person() {
        assert_eq!(
            dm("D1", None, &["U2"]).display(&labels(), SELF),
            "@William Riker"
        );
    }

    /// A group DM's `members` includes the account itself. Titling it
    /// with your own name in the list is not what anyone means by "who
    /// is this conversation with".
    #[test]
    fn a_group_dm_names_the_others() {
        assert_eq!(
            dm(
                "G1",
                Some("mpdm-picard--riker--data-1"),
                &["U1", "U2", "U3"]
            )
            .display(&labels(), SELF),
            "@William Riker, Data"
        );
    }

    #[test]
    fn a_dm_with_an_unknown_user_falls_back_to_the_user_id() {
        assert_eq!(dm("D9", None, &["U404"]).display(&labels(), SELF), "@U404");
    }

    /// A store written before `dm_user_ids` existed, or a shape without
    /// participants: Slack's own composite handle, then the raw id.
    #[test]
    fn a_dm_without_participants_falls_back_to_the_handle_then_the_id() {
        assert_eq!(
            dm("G1", Some("mpdm-picard--riker--data-1"), &[]).display(&labels(), SELF),
            "@mpdm-picard--riker--data-1"
        );
        assert_eq!(dm("D9", None, &[]).display(&labels(), SELF), "D9");
    }

    /// A DM with yourself still has to be nameable.
    #[test]
    fn a_note_to_self_keeps_your_own_name() {
        assert_eq!(
            dm("D0", None, &["U1"]).display(&labels(), SELF),
            "@Jean-Luc Picard"
        );
    }
}
