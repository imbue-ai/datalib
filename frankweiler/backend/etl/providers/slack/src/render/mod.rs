//! Slack render stage: raw → typed buckets ready for render.
//!
//! Entry point is [`parse::parse`]: it opens the doltlite DB, runs
//! the `dolt_diff_<table>` scan against the render cursor, and
//! loads only the changed thread buckets — each one carrying its
//! own per-thread [`frankweiler_etl::blob_cas::BlobBundle`] so render
//! is fully sync. Falls back to the legacy JSON-tree reader for the
//! in-crate fixture (cold-start only, every thread rendered).
//!
//! Determinism: row UUIDs are `uuid::Uuid::new_v5` with the slack
//! namespace defined in `download::schema_raw`. Same hash for the same
//! source data across re-ingest.

pub mod mrkdwn;
pub mod parse;
pub mod render;

use std::collections::BTreeMap;

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
        self.real_name
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| self.user_id.clone())
    }
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub channel_id: String,
    pub name: Option<String>,
    pub is_im: bool,
    pub is_mpim: bool,
    pub user_id: Option<String>,
    pub member_ids: Vec<String>,
    pub purpose: Option<String>,
}

impl Channel {
    pub(crate) fn from_raw(channel_id: String, name: Option<String>, raw: &Value) -> Self {
        let member_ids = raw
            .get("members")
            .and_then(Value::as_array)
            .map(|members| {
                members
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            channel_id,
            name,
            is_im: raw.get("is_im").and_then(Value::as_bool) == Some(true),
            is_mpim: raw.get("is_mpim").and_then(Value::as_bool) == Some(true),
            user_id: raw.get("user").and_then(Value::as_str).map(str::to_string),
            member_ids,
            purpose: raw
                .get("purpose")
                .and_then(|v| v.get("value"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    pub fn display(&self, users: &BTreeMap<String, User>, self_user_id: Option<&str>) -> String {
        let label_for = |id: &str| {
            users
                .get(id)
                .map(|user| {
                    user.display_name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| user.label())
                })
                .unwrap_or_else(|| id.to_string())
        };

        if self.is_im {
            return self
                .user_id
                .as_deref()
                .map(|id| format!("DM with {}", label_for(id)))
                .unwrap_or_else(|| format!("Direct message ({})", self.channel_id));
        }

        if self.is_mpim {
            let mut participants: Vec<String> = self
                .member_ids
                .iter()
                .filter(|id| self_user_id != Some(id.as_str()))
                .map(|id| label_for(id))
                .collect();
            participants.sort();
            participants.dedup();
            if !participants.is_empty() {
                return format!("Group DM with {}", participants.join(", "));
            }
            if let Some(purpose) = self
                .purpose
                .as_deref()
                .and_then(|p| p.strip_prefix("Group messaging with: "))
                .filter(|p| !p.trim().is_empty())
            {
                return format!("Group DM with {purpose}");
            }
            if let Some(participants) = self
                .name
                .as_deref()
                .and_then(humanize_mpim_name)
                .filter(|p| !p.is_empty())
            {
                return format!("Group DM with {participants}");
            }
            return format!("Group direct message ({})", self.channel_id);
        }

        format!(
            "#{}",
            self.name.clone().unwrap_or_else(|| self.channel_id.clone())
        )
    }
}

fn humanize_mpim_name(name: &str) -> Option<String> {
    let raw = name.strip_prefix("mpdm-")?;
    let without_counter = raw
        .rsplit_once('-')
        .filter(|(_, suffix)| suffix.chars().all(|c| c.is_ascii_digit()))
        .map(|(head, _)| head)
        .unwrap_or(raw);
    Some(without_counter.split("--").collect::<Vec<_>>().join(", "))
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
mod tests {
    use super::*;

    fn user(id: &str, display_name: &str) -> User {
        User {
            user_id: id.to_string(),
            team_id: "T1".to_string(),
            name: None,
            real_name: None,
            display_name: Some(display_name.to_string()),
        }
    }

    #[test]
    fn direct_message_labels_resolve_participants() {
        let users = BTreeMap::from([
            ("U1".to_string(), user("U1", "Alice")),
            ("U2".to_string(), user("U2", "Bob")),
            ("U3".to_string(), user("U3", "Carol")),
        ]);
        let im = Channel::from_raw(
            "D1".to_string(),
            None,
            &serde_json::json!({"is_im": true, "user": "U2"}),
        );
        assert_eq!(im.display(&users, Some("U1")), "DM with Bob");

        let mpim = Channel::from_raw(
            "G1".to_string(),
            Some("mpdm-alice--bob--carol-1".to_string()),
            &serde_json::json!({
                "is_mpim": true,
                "members": ["U1", "U2", "U3"],
            }),
        );
        assert_eq!(mpim.display(&users, Some("U1")), "Group DM with Bob, Carol");
    }

    #[test]
    fn group_dm_label_falls_back_to_generated_name() {
        let mpim = Channel::from_raw(
            "G1".to_string(),
            Some("mpdm-alice--bob--carol-2".to_string()),
            &serde_json::json!({"is_mpim": true}),
        );
        assert_eq!(
            mpim.display(&BTreeMap::new(), None),
            "Group DM with alice, bob, carol"
        );
    }
}
