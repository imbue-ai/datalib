//! Slack translate stage: raw → typed buckets ready for render.
//!
//! Entry point is [`parse::parse`]: it opens the doltlite DB, runs
//! the `dolt_diff_<table>` scan against the render cursor, and
//! loads only the changed thread buckets — each one carrying its
//! own per-thread [`frankweiler_etl::blob_cas::BlobBundle`] so render
//! is fully sync. Falls back to the legacy JSON-tree reader for the
//! in-crate fixture (cold-start only, every thread rendered).
//!
//! Determinism: row UUIDs are `uuid::Uuid::new_v5` with the slack
//! namespace defined in `extract::schema_raw`. Same hash for the same
//! source data across re-ingest.

pub mod mrkdwn;
pub mod parse;
pub mod render;

use std::collections::BTreeMap;

use chrono::{DateTime, TimeZone, Utc};
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

// UUIDv5 recipes for Slack message and thread ids live in
// `extract::schema_raw`. Re-export here so existing
// `crate::translate::slack_message_uuid` callers outside this crate
// keep resolving.
pub use super::extract::schema_raw::{slack_message_uuid, slack_thread_uuid};
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

// ---------------------------------------------------------------------------
// Grid row emission (cross-provider). Used by callers that want just
// the grid_rows projection without rendering markdown.
// ---------------------------------------------------------------------------

pub fn grid_rows(t: &ParsedSlack) -> Vec<GridRow> {
    let user_labels: BTreeMap<String, String> = t
        .users
        .iter()
        .map(|(id, u)| (id.clone(), u.label()))
        .collect();
    let mut out: Vec<GridRow> = Vec::new();
    for bucket in &t.threads {
        let root: &Message = bucket
            .messages
            .iter()
            .find(|m| m.is_thread_root)
            .unwrap_or_else(|| bucket.messages.first().expect("non-empty thread bucket"));
        let channel = t.channels.get(&root.channel_id);
        let cname = channel
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| root.channel_id.clone());
        let thread_uuid = bucket.thread_uuid.clone();

        out.push(GridRow {
            uuid: thread_uuid.clone(),
            provider: "slack".to_string(),
            kind: "Slack Thread".to_string(),
            source_label: "Slack".to_string(),
            when_ts: Some(root.ts_iso.clone()),
            author: root
                .user_id
                .as_deref()
                .and_then(|u| user_labels.get(u).cloned())
                .or_else(|| root.user_id.clone()),
            account: Some(root.team_id.clone()),
            org_uuid: None,
            org_name: None,
            project: None,
            channel: Some(cname.clone()),
            conversation_name: Some(format!("#{cname}")),
            conversation_uuid: thread_uuid.clone(),
            message_index: None,
            entire_chat: format!("/slack/{thread_uuid}"),
            text: resolve_user_mentions(&root.text, &user_labels),
            slack_link: Some(slack_link(&root.team_id, &root.channel_id, &root.ts, None)),
            qmd_path: Some(slack_qmd_path(
                &root.team_id,
                &root.channel_id,
                &thread_uuid,
            )),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(thread_uuid.clone()),
        });

        for (idx, m) in bucket.messages.iter().enumerate() {
            out.push(GridRow {
                uuid: m.uuid(),
                provider: "slack".to_string(),
                kind: "Slack Message".to_string(),
                source_label: "Slack".to_string(),
                when_ts: Some(m.ts_iso.clone()),
                author: m
                    .user_id
                    .as_deref()
                    .and_then(|u| user_labels.get(u).cloned())
                    .or_else(|| m.user_id.clone()),
                account: Some(m.team_id.clone()),
                org_uuid: None,
                org_name: None,
                project: None,
                channel: Some(cname.clone()),
                conversation_name: Some(format!("#{cname}")),
                conversation_uuid: thread_uuid.clone(),
                message_index: Some(idx as i64),
                entire_chat: format!("/slack/{thread_uuid}"),
                text: resolve_user_mentions(&m.text, &user_labels),
                slack_link: Some(slack_link(&m.team_id, &m.channel_id, &m.ts, Some(&root.ts))),
                qmd_path: Some(slack_qmd_path(
                    &root.team_id,
                    &root.channel_id,
                    &thread_uuid,
                )),
                source_url: None,
                git_sha: None,
                external_id: None,
                notion_page_uuid: None,
                notion_block_uuid: None,
                markdown_uuid: Some(thread_uuid.clone()),
            });
        }
    }
    out
}

pub use mrkdwn::resolve_user_mentions;

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

pub fn slack_qmd_path(team_id: &str, channel_id: &str, thread_uuid: &str) -> String {
    format!("rendered_md/slack/{team_id}/{channel_id}/threads/{thread_uuid}/index.md")
}
