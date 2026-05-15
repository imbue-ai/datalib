//! Slack translate stage: raw API capture → grid_rows projection.
//!
//! Reads `<root>/raw_api/<method>/*.jsonl` (the layout written by
//! `raw_store::RawStore`) and emits one `GridRow` per thread + one per
//! message, matching the codegen'd `grid_rows` schema. Workspace, users,
//! and channels are held in in-memory lookup tables only — they don't
//! land in grid_rows themselves, they just supply labels/UUIDs for the
//! rows that do.
//!
//! Determinism: row UUIDs are `uuid::Uuid::new_v5` with the same Slack
//! namespace the Python translator uses
//! (`a89c7c4f-3e3d-5a6b-9f8a-3e3d5a6b9f8a`), so a Rust-translated row
//! collides on `uuid` with a Python-translated row from the same source.
//! That's the whole point — we want the cutover to be a write-through
//! swap, not a re-keying of every existing grid_rows entry.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use uuid::Uuid;

use frankweiler_schema::grid_rows::GridRow;

use super::shapes::{M_AUTH_TEST, M_CHANNELS, M_HISTORY, M_REPLIES, M_USERS};

/// Shared namespace for v5-derived Slack UUIDs. Must match the Python
/// constant in `src/ingest/providers/slack/parse.py`.
const SLACK_UUID_NS: Uuid = Uuid::from_bytes([
    0xa8, 0x9c, 0x7c, 0x4f, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

pub fn slack_message_uuid(team_id: &str, channel_id: &str, ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:msg:{team_id}:{channel_id}:{ts}").as_bytes(),
    )
    .to_string()
}

pub fn slack_thread_uuid(team_id: &str, channel_id: &str, thread_ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:thread:{team_id}:{channel_id}:{thread_ts}").as_bytes(),
    )
    .to_string()
}

/// Render Slack `ts` (unix seconds + fractional, UTC) as ISO-8601 with
/// microsecond precision and `+00:00` offset — matches Python's
/// `datetime.fromtimestamp(float(ts), tz=utc).isoformat(timespec='microseconds')`.
pub fn ts_to_iso(ts: &str) -> String {
    // Slack `ts` is "<unix_seconds>.<6-digit-fractional>". We avoid
    // parsing as f64 because integer seconds beyond ~10^11 (anything
    // post-Y5138, which sounds far off but is the very dating that
    // makes our 2369-stardate test fixtures legible) silently lose
    // microsecond precision in IEEE 754.
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
    /// reaches into this for `files`, `reactions`, and any future field
    /// we don't promote to a struct member.
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

#[derive(Debug, Default)]
pub struct TranslatedSlack {
    pub workspace: Option<Workspace>,
    pub users: BTreeMap<String, User>,
    pub channels: BTreeMap<String, Channel>,
    /// Keyed by `(channel_id, ts)` so cross-stream duplicates collapse.
    pub messages: BTreeMap<(String, String), Message>,
}

impl TranslatedSlack {
    pub fn fallback_team_id(&self) -> &str {
        self.workspace
            .as_ref()
            .map(|w| w.team_id.as_str())
            .unwrap_or("unknown")
    }
}

/// Iterate every envelope across every `raw_api/<method>/*.jsonl` file
/// for `method`, lexicographic by filename (run stamps sort naturally).
fn read_method_envelopes(raw_dir: &Path, method: &str) -> Result<Vec<Value>> {
    let dir = raw_dir.join(method);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value =
                serde_json::from_str(&line).with_context(|| format!("parse {}", path.display()))?;
            out.push(v);
        }
    }
    Ok(out)
}

pub fn translate_raw_dir(out_dir: &Path) -> Result<TranslatedSlack> {
    let raw_dir = out_dir.join("raw_api");
    let mut t = TranslatedSlack::default();

    // --- workspace (auth.test) -------------------------------------------
    for env in read_method_envelopes(&raw_dir, M_AUTH_TEST)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        let team_id = str_or(&resp, "team_id");
        if team_id.is_empty() {
            continue;
        }
        t.workspace = Some(Workspace {
            team_id: team_id.clone(),
            team_name: opt_str(&resp, "team"),
            team_url: opt_str(&resp, "url"),
            self_user_id: opt_str(&resp, "user_id"),
        });
    }
    let team_id = t.fallback_team_id().to_string();

    // --- users -----------------------------------------------------------
    for env in read_method_envelopes(&raw_dir, M_USERS)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for u in array_field(&resp, "members") {
            let id = str_or(u, "id");
            if id.is_empty() {
                continue;
            }
            let profile = u.get("profile");
            t.users.insert(
                id.clone(),
                User {
                    user_id: id,
                    team_id: opt_str(u, "team_id").unwrap_or_else(|| team_id.clone()),
                    name: opt_str(u, "name"),
                    real_name: opt_str(u, "real_name")
                        .or_else(|| profile.and_then(|p| opt_str(p, "real_name"))),
                    display_name: profile.and_then(|p| opt_str(p, "display_name")),
                },
            );
        }
    }

    // --- channels --------------------------------------------------------
    for env in read_method_envelopes(&raw_dir, M_CHANNELS)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for c in array_field(&resp, "channels") {
            let id = str_or(c, "id");
            if id.is_empty() {
                continue;
            }
            t.channels.insert(
                id.clone(),
                Channel {
                    channel_id: id,
                    name: opt_str(c, "name"),
                },
            );
        }
    }

    // --- history (top-level messages) ------------------------------------
    for env in read_method_envelopes(&raw_dir, M_HISTORY)? {
        let params = env.get("params");
        let channel_id = params
            .and_then(|p| p.get("channel"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() {
            continue;
        }
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for m in array_field(&resp, "messages") {
            let ts = str_or(m, "ts");
            if ts.is_empty() {
                continue;
            }
            let thread_ts = opt_str(m, "thread_ts");
            let effective = thread_ts.clone().unwrap_or_else(|| ts.clone());
            let is_root = match &thread_ts {
                None => true,
                Some(t_ts) => t_ts == &ts,
            };
            insert_message(
                &mut t.messages,
                &team_id,
                &channel_id,
                &ts,
                thread_ts,
                effective,
                is_root,
                m,
            );
        }
    }

    // --- replies (threaded children, plus the parent re-served) ----------
    for env in read_method_envelopes(&raw_dir, M_REPLIES)? {
        let params = env.get("params");
        let channel_id = params
            .and_then(|p| p.get("channel"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let req_thread_ts = params
            .and_then(|p| p.get("ts"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() || req_thread_ts.is_empty() {
            continue;
        }
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for m in array_field(&resp, "messages") {
            let ts = str_or(m, "ts");
            if ts.is_empty() {
                continue;
            }
            // Slack returns the parent inline with replies; treat
            // thread_ts == ts as the root regardless of which endpoint
            // delivered it.
            let thread_ts = opt_str(m, "thread_ts").or_else(|| Some(req_thread_ts.clone()));
            let effective = thread_ts.clone().unwrap_or_else(|| ts.clone());
            let is_root = ts == req_thread_ts;
            insert_message(
                &mut t.messages,
                &team_id,
                &channel_id,
                &ts,
                thread_ts,
                effective,
                is_root,
                m,
            );
        }
    }

    Ok(t)
}

#[allow(clippy::too_many_arguments)]
fn insert_message(
    out: &mut BTreeMap<(String, String), Message>,
    team_id: &str,
    channel_id: &str,
    ts: &str,
    thread_ts: Option<String>,
    effective_thread_ts: String,
    is_thread_root: bool,
    raw: &Value,
) {
    let key = (channel_id.to_string(), ts.to_string());
    if out.contains_key(&key) {
        // First-writer-wins: history captures land first and carry the
        // top-level fields; replies confirm the same row.
        return;
    }
    let msg = Message {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        effective_thread_ts,
        is_thread_root,
        user_id: opt_str(raw, "user"),
        text: opt_str(raw, "text").unwrap_or_default(),
        ts_iso: ts_to_iso(ts),
        raw_json: raw.clone(),
    };
    out.insert(key, msg);
}

// ---------------------------------------------------------------------------
// Grid row emission.
// ---------------------------------------------------------------------------

pub fn grid_rows(t: &TranslatedSlack) -> Vec<GridRow> {
    let user_labels: BTreeMap<String, String> = t
        .users
        .iter()
        .map(|(id, u)| (id.clone(), u.label()))
        .collect();

    // Group messages by thread_uuid, preserving ordering within a thread.
    let mut by_thread: BTreeMap<String, Vec<&Message>> = BTreeMap::new();
    for m in t.messages.values() {
        by_thread.entry(m.thread_uuid()).or_default().push(m);
    }

    let mut out: Vec<GridRow> = Vec::new();
    for (thread_uuid, mut msgs) in by_thread {
        msgs.sort_by(|a, b| {
            (a.ts_iso.as_str(), a.ts.as_str()).cmp(&(b.ts_iso.as_str(), b.ts.as_str()))
        });
        let root: &Message = msgs
            .iter()
            .copied()
            .find(|m| m.is_thread_root)
            .unwrap_or(msgs[0]);
        let channel = t.channels.get(&root.channel_id);
        let cname = channel
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| root.channel_id.clone());
        let author_label = root
            .user_id
            .as_deref()
            .and_then(|u| user_labels.get(u).cloned())
            .or_else(|| root.user_id.clone());

        // Thread row.
        out.push(GridRow {
            uuid: thread_uuid.clone(),
            provider: "slack".to_string(),
            kind: "Slack Thread".to_string(),
            source_label: "Slack".to_string(),
            when_ts: root.ts_iso.clone(),
            author: author_label.clone(),
            account: Some(root.team_id.clone()),
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
                &cname,
                &thread_uuid,
                &root.text,
                &user_labels,
            )),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            document_uuid: Some(thread_uuid.clone()),
        });

        // Message rows.
        for (idx, m) in msgs.iter().enumerate() {
            let mauthor = m
                .user_id
                .as_deref()
                .and_then(|u| user_labels.get(u).cloned())
                .or_else(|| m.user_id.clone());
            out.push(GridRow {
                uuid: m.uuid(),
                provider: "slack".to_string(),
                kind: "Slack Message".to_string(),
                source_label: "Slack".to_string(),
                when_ts: m.ts_iso.clone(),
                author: mauthor,
                account: Some(m.team_id.clone()),
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
                    &cname,
                    &thread_uuid,
                    &root.text,
                    &user_labels,
                )),
                source_url: None,
                git_sha: None,
                external_id: None,
                notion_page_uuid: None,
                notion_block_uuid: None,
                document_uuid: Some(thread_uuid.clone()),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers: mention resolution, slug, slack-link, qmd path.
// ---------------------------------------------------------------------------

// Mention resolution is shared with the renderer in `mrkdwn` — that
// keeps the grid-row text and the .md preview spelling identical.
pub use super::mrkdwn::resolve_user_mentions;

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

const SLUG_MAX_LEN: usize = 60;

fn slugify(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    let mut prev_dash = true;
    for ch in name.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            s.push(c);
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        return "untitled".to_string();
    }
    if s.len() > SLUG_MAX_LEN {
        s.truncate(SLUG_MAX_LEN);
        while s.ends_with('-') {
            s.pop();
        }
        if s.is_empty() {
            return "untitled".to_string();
        }
    }
    s
}

fn slack_thread_title(root_text: &str, user_labels: &BTreeMap<String, String>) -> String {
    let resolved = resolve_user_mentions(root_text, user_labels);
    let first = resolved
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty thread)")
        .to_string();
    first.chars().take(80).collect()
}

pub fn slack_qmd_path(
    team_id: &str,
    channel_name: &str,
    thread_uuid: &str,
    root_text: &str,
    user_labels: &BTreeMap<String, String>,
) -> String {
    let slug = slugify(&slack_thread_title(root_text, user_labels));
    format!("rendered_md/slack/{team_id}/{channel_name}/threads/{thread_uuid}__{slug}.md")
}

// ---------------------------------------------------------------------------
// JSON tiny helpers.
// ---------------------------------------------------------------------------

fn str_or(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}
fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}
fn array_field<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}
