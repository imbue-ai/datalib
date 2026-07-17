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
