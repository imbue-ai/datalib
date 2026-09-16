//! One transcript file → its records and what the file says about
//! itself. A transcript is JSONL: every line is one record with a `type`.
//! The content-bearing types (`user`, `assistant`, `system`) carry a
//! `uuid` and become rows; the bookkeeping types (`custom-title`,
//! `bridge-session`, `pr-link`, …) describe the session and fold into
//! its transcript row. Anything unparsable is counted, not fatal.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

/// The record types stored as rows. Everything else folds or is dropped.
pub const CONTENT_TYPES: &[&str] = &["user", "assistant", "system"];

#[derive(Debug, Clone)]
pub struct ParsedTranscript {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub meta: TranscriptMeta,
    pub records: Vec<ParsedRecord>,
    pub stats: ParseStats,
}

impl ParsedTranscript {
    /// The `transcripts.id`: the session id, or `<session>#<agent>` for a
    /// subagent transcript, whose records carry the parent's session id.
    pub fn transcript_id(&self) -> String {
        transcript_id(&self.session_id, self.agent_id.as_deref())
    }
}

pub fn transcript_id(session_id: &str, agent_id: Option<&str>) -> String {
    match agent_id {
        Some(a) => format!("{session_id}#{a}"),
        None => session_id.to_string(),
    }
}

/// What the file says about the session as a whole; stored as the
/// transcript row's payload.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TranscriptMeta {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub rel_path: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub git_branches: BTreeSet<String>,
    pub title: Option<String>,
    /// Where `title` came from: `custom` (the person renamed it), `ai`
    /// (Claude Code named it), `agent` (a subagent's name), or `prompt`
    /// (the first thing the person typed).
    pub title_source: Option<&'static str>,
    pub first_prompt: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub entrypoints: BTreeSet<String>,
    pub versions: BTreeSet<String>,
    pub models: BTreeSet<String>,
    /// The cloud session this local transcript is bridged to, when the
    /// desktop app or `--teleport` linked them.
    pub cloud_session_id: Option<String>,
    pub org_uuid: Option<String>,
    pub account_uuid: Option<String>,
    pub pr_links: Vec<Value>,
    pub record_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct ParsedRecord {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub record_type: String,
    pub timestamp: Option<String>,
    pub is_sidechain: bool,
    pub raw: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseStats {
    pub lines: usize,
    pub records: usize,
    pub folded: usize,
    /// Content records with no `uuid`, which cannot be keyed.
    pub unkeyed: usize,
    pub malformed: usize,
}

/// `None` when no line names a session — an empty or foreign file.
pub fn parse_transcript(
    text: &str,
    rel_path: &str,
    agent_id_from_path: Option<&str>,
) -> Option<ParsedTranscript> {
    let mut meta = TranscriptMeta {
        rel_path: rel_path.to_string(),
        ..Default::default()
    };
    let mut records = Vec::new();
    let mut stats = ParseStats::default();
    let mut session_id: Option<String> = None;
    let mut agent_id: Option<String> = agent_id_from_path.map(str::to_string);
    let mut custom_title = None;
    let mut ai_title = None;
    let mut agent_name = None;

    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines += 1;
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                stats.malformed += 1;
                continue;
            }
        };
        let Some(record_type) = v.get("type").and_then(Value::as_str) else {
            stats.malformed += 1;
            continue;
        };
        *meta
            .record_counts
            .entry(record_type.to_string())
            .or_default() += 1;
        if session_id.is_none() {
            session_id = str_field(&v, "sessionId");
        }
        if agent_id.is_none() {
            agent_id = str_field(&v, "agentId");
        }

        if CONTENT_TYPES.contains(&record_type) {
            let Some(uuid) = str_field(&v, "uuid") else {
                stats.unkeyed += 1;
                continue;
            };
            note_record_facts(&mut meta, &v);
            if record_type == "user" && meta.first_prompt.is_none() {
                meta.first_prompt = first_prompt(&v);
            }
            records.push(ParsedRecord {
                uuid,
                parent_uuid: str_field(&v, "parentUuid"),
                record_type: record_type.to_string(),
                timestamp: str_field(&v, "timestamp"),
                is_sidechain: v
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                raw: v,
            });
            stats.records += 1;
            continue;
        }

        stats.folded += 1;
        match record_type {
            "custom-title" => custom_title = str_field(&v, "customTitle"),
            "ai-title" => ai_title = str_field(&v, "aiTitle"),
            "agent-name" => agent_name = str_field(&v, "agentName"),
            "bridge-session" => {
                meta.cloud_session_id = str_field(&v, "bridgeSessionId");
                meta.org_uuid = str_field(&v, "ownerOrganizationUuid");
                meta.account_uuid = str_field(&v, "ownerAccountUuid");
            }
            "pr-link" => meta.pr_links.push(v),
            _ => {}
        }
    }

    let session_id = session_id?;
    meta.session_id = session_id.clone();
    meta.agent_id = agent_id.clone();
    (meta.title, meta.title_source) = match (custom_title, ai_title, agent_name) {
        (Some(t), _, _) => (Some(t), Some("custom")),
        (None, Some(t), _) => (Some(t), Some("ai")),
        (None, None, Some(t)) => (Some(t), Some("agent")),
        (None, None, None) => match meta.first_prompt.clone() {
            Some(p) => (Some(p), Some("prompt")),
            None => (None, None),
        },
    };
    Some(ParsedTranscript {
        session_id,
        agent_id,
        meta,
        records,
        stats,
    })
}

fn note_record_facts(meta: &mut TranscriptMeta, v: &Value) {
    if meta.cwd.is_none() {
        meta.cwd = str_field(v, "cwd");
    }
    if let Some(b) = str_field(v, "gitBranch") {
        if meta.git_branch.is_none() {
            meta.git_branch = Some(b.clone());
        }
        meta.git_branches.insert(b);
    }
    if let Some(e) = str_field(v, "entrypoint") {
        meta.entrypoints.insert(e);
    }
    if let Some(ver) = str_field(v, "version") {
        meta.versions.insert(ver);
    }
    if let Some(m) = v
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        meta.models.insert(m.to_string());
    }
    if let Some(ts) = str_field(v, "timestamp") {
        if meta.started_at.as_deref().is_none_or(|s| ts.as_str() < s) {
            meta.started_at = Some(ts.clone());
        }
        if meta.updated_at.as_deref().is_none_or(|s| ts.as_str() > s) {
            meta.updated_at = Some(ts);
        }
    }
}

/// The first line of what the person typed, for a title when nothing
/// named the session. Tool results and the harness's own `isMeta`
/// messages are `user` records too and are not prompts.
fn first_prompt(v: &Value) -> Option<String> {
    if v.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let text = human_text(v.get("message")?.get("content")?)?;
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    Some(truncate_chars(line, 100))
}

/// The text of a user record's content, or `None` when it is a tool
/// result or has no text.
pub fn human_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = b.get("text").and_then(Value::as_str) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("tool_result") => return None,
                    _ => {}
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        _ => None,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The agent id a subagent transcript's file name carries:
/// `subagents/agent-<id>.jsonl`.
pub fn agent_id_from_path(rel_path: &str) -> Option<&str> {
    let name = rel_path.rsplit('/').next()?;
    let stem = name.strip_suffix(".jsonl")?;
    stem.strip_prefix("agent-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(v: Value) -> String {
        v.to_string()
    }

    fn user(uuid: &str, ts: &str, content: Value) -> Value {
        json!({
            "type": "user", "uuid": uuid, "parentUuid": null, "sessionId": "s1",
            "timestamp": ts, "cwd": "/src/enterprise", "gitBranch": "main",
            "version": "2.1.270", "entrypoint": "cli", "isSidechain": false,
            "message": {"role": "user", "content": content}
        })
    }

    #[test]
    fn content_records_become_rows_and_bookkeeping_folds() {
        let text = [
            line(user("u1", "2364-04-11T10:00:00.000Z", json!("Realign the deflector dish"))),
            line(json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": "s1",
                "timestamp": "2364-04-11T10:00:05.000Z", "cwd": "/src/enterprise",
                "message": {"role": "assistant", "model": "claude-opus-5", "content": [{"type": "text", "text": "Aye."}]}})),
            line(json!({"type": "custom-title", "customTitle": "Deflector realignment", "sessionId": "s1"})),
            line(json!({"type": "last-prompt", "lastPrompt": "x", "sessionId": "s1"})),
            line(json!({"type": "bridge-session", "sessionId": "s1", "bridgeSessionId": "cse_01", "ownerOrganizationUuid": "org-1", "ownerAccountUuid": "acct-1"})),
            "not json at all".to_string(),
        ]
        .join("\n");
        let t = parse_transcript(&text, "p/s1.jsonl", None).unwrap();
        assert_eq!(t.session_id, "s1");
        assert_eq!(t.agent_id, None);
        assert_eq!(t.transcript_id(), "s1");
        assert_eq!(t.records.len(), 2);
        assert_eq!(
            t.stats,
            ParseStats {
                lines: 6,
                records: 2,
                folded: 3,
                unkeyed: 0,
                malformed: 1
            }
        );
        assert_eq!(t.meta.title.as_deref(), Some("Deflector realignment"));
        assert_eq!(t.meta.title_source, Some("custom"));
        assert_eq!(
            t.meta.first_prompt.as_deref(),
            Some("Realign the deflector dish")
        );
        assert_eq!(t.meta.cloud_session_id.as_deref(), Some("cse_01"));
        assert_eq!(
            t.meta.started_at.as_deref(),
            Some("2364-04-11T10:00:00.000Z")
        );
        assert_eq!(
            t.meta.updated_at.as_deref(),
            Some("2364-04-11T10:00:05.000Z")
        );
        assert!(t.meta.models.contains("claude-opus-5"));
        assert_eq!(t.meta.record_counts["last-prompt"], 1);
    }

    /// A session nobody named is titled by its first prompt, never by a
    /// tool result or a harness-injected `isMeta` message.
    #[test]
    fn untitled_session_takes_the_first_real_prompt() {
        let text = [
            line(
                json!({"type": "user", "uuid": "m0", "sessionId": "s1", "isMeta": true,
                "message": {"content": "Caveat: the harness said this"}}),
            ),
            line(user(
                "r0",
                "2364-04-11T10:00:00.000Z",
                json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
            )),
            line(user(
                "u1",
                "2364-04-11T10:00:01.000Z",
                json!("Scan for\nlifeforms"),
            )),
        ]
        .join("\n");
        let t = parse_transcript(&text, "p/s1.jsonl", None).unwrap();
        assert_eq!(t.meta.title.as_deref(), Some("Scan for"));
        assert_eq!(t.meta.title_source, Some("prompt"));
    }

    #[test]
    fn subagent_id_comes_from_the_records_or_the_file_name() {
        let text = line(
            json!({"type": "user", "uuid": "u1", "sessionId": "s1", "agentId": "a9",
            "isSidechain": true, "message": {"content": "hi"}}),
        );
        let t = parse_transcript(&text, "p/s1/subagents/agent-a9.jsonl", None).unwrap();
        assert_eq!(t.transcript_id(), "s1#a9");
        assert!(t.records[0].is_sidechain);
        assert_eq!(
            agent_id_from_path("p/s1/subagents/agent-a9.jsonl"),
            Some("a9")
        );
        assert_eq!(agent_id_from_path("p/s1.jsonl"), None);
    }

    #[test]
    fn a_file_naming_no_session_is_not_a_transcript() {
        assert!(parse_transcript("", "x.jsonl", None).is_none());
        assert!(parse_transcript("{\"type\":\"summary\"}", "x.jsonl", None).is_none());
    }

    #[test]
    fn a_content_record_without_a_uuid_is_counted_not_stored() {
        let text = line(json!({"type": "user", "sessionId": "s1", "message": {"content": "x"}}));
        let t = parse_transcript(&text, "p/s1.jsonl", None).unwrap();
        assert!(t.records.is_empty());
        assert_eq!(t.stats.unkeyed, 1);
    }
}
