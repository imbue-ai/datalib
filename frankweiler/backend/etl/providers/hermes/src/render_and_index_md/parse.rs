//! Parse a Hermes / OpenClaw-compatible export directory into structured
//! sessions. Three on-disk shapes are accepted (see
//! `.hermes/plans/hermes-openclaw-support.md` and the rich-context doc):
//!
//! * **A. Session-export JSONL** — `*.jsonl`, one message/event object per
//!   line, each carrying its `session_id`; session-level metadata (title,
//!   surface, model, …) piggybacks on the records that have it.
//! * **B. Session snapshot JSON** — `*.json` object with top-level session
//!   metadata and a `messages: [...]` array.
//! * **C. Generic OpenClaw-compatible records** — the same JSONL/JSON shapes
//!   but with alias keys: `conversation_id` / `thread_id` for the session id,
//!   `author.role` for the role, `text` for content, `created_at` for the
//!   timestamp.
//!
//! The parser is deliberately permissive (every field optional, coerced from
//! `serde_json::Value`) so a partially-populated export still yields a usable
//! transcript rather than a hard parse error. Rewound (`active = 0`) and
//! compressed (`compacted = 1`) messages are dropped from the normal
//! transcript, per the Hermes state semantics.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

/// A whole export directory, in first-appearance session order.
#[derive(Debug, Default)]
pub struct ParsedHermesExport {
    pub sessions: Vec<HermesSession>,
}

/// One Hermes session (conversation).
#[derive(Debug, Default)]
pub struct HermesSession {
    pub id: String,
    pub title: Option<String>,
    /// Platform surface: `cli`, `telegram`, `discord`, `cron`, `acp`, …
    pub source: Option<String>,
    pub model: Option<String>,
    pub user_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub started_at_ms: Option<i64>,
    pub messages: Vec<HermesMessage>,
}

/// One message within a session, already coerced to display-ready fields.
#[derive(Debug, Default)]
pub struct HermesMessage {
    pub role: String,
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_name: Option<String>,
    /// Pretty-printed `tool_calls` JSON, when present on the record.
    pub tool_calls_pretty: Option<String>,
    /// Per-message model/provider override (assistant turns).
    pub model: Option<String>,
    pub timestamp_ms: Option<i64>,
}

// ─────────────────────────────────────────────────────────────────────
// Raw serde shapes
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct Author {
    #[serde(default)]
    role: Option<String>,
}

/// A single record — a JSONL line, an element of a JSON array, or a message
/// inside a snapshot's `messages`. Session-level metadata fields are read off
/// whichever records carry them.
#[derive(Debug, Default, Deserialize)]
struct RawRecord {
    #[serde(default, alias = "conversation_id", alias = "thread_id")]
    session_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default, alias = "text")]
    content: Option<Value>,
    #[serde(default, alias = "created_at", alias = "ts")]
    timestamp: Option<Value>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_calls: Option<Value>,
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    model: Option<String>,
    // Session-level metadata (may ride along on any record).
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "user")]
    user_id: Option<String>,
    #[serde(default)]
    started_at: Option<Value>,
    #[serde(default)]
    parent_session_id: Option<String>,
    // Hermes state flags. Integer (0/1) in the canonical store, but accept a
    // JSON bool too.
    #[serde(default)]
    active: Option<Value>,
    #[serde(default)]
    compacted: Option<Value>,
}

/// A snapshot object: session metadata + a `messages` array.
#[derive(Debug, Default, Deserialize)]
struct RawSnapshot {
    #[serde(
        default,
        alias = "session_id",
        alias = "conversation_id",
        alias = "thread_id"
    )]
    id: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, alias = "user")]
    user_id: Option<String>,
    #[serde(default)]
    started_at: Option<Value>,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    messages: Vec<RawRecord>,
}

// ─────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────

/// Parse every `*.jsonl` / `*.json` file under `dir` (recursively) into
/// sessions. Files are visited in sorted path order and sessions preserve
/// first-appearance order, so the result is deterministic.
pub fn parse_export_dir(dir: &Path) -> Result<ParsedHermesExport> {
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    collect_files(dir, &mut files)?;
    files.sort();

    let mut acc = SessionAccumulator::default();
    for path in &files {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let is_jsonl = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("jsonl"))
            .unwrap_or(false);
        if is_jsonl {
            parse_jsonl(&raw, path, &mut acc)?;
        } else {
            parse_json(&raw, path, &mut acc)?;
        }
    }
    Ok(ParsedHermesExport {
        sessions: acc.into_sessions(),
    })
}

fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if !dir.exists() {
        // A missing export dir yields an empty parse rather than an error — the
        // orchestrator only schedules this source when input_path is set, and a
        // not-yet-populated dir shouldn't fail the whole run.
        tracing::warn!(dir = %dir.display(), "hermes export dir does not exist; nothing to parse");
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json") {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn parse_jsonl(raw: &str, path: &Path, acc: &mut SessionAccumulator) -> Result<()> {
    for (lineno, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: RawRecord = serde_json::from_str(line)
            .with_context(|| format!("{}:{}: parse jsonl record", path.display(), lineno + 1))?;
        acc.ingest_record(rec);
    }
    Ok(())
}

fn parse_json(raw: &str, path: &Path, acc: &mut SessionAccumulator) -> Result<()> {
    let value: Value =
        serde_json::from_str(raw).with_context(|| format!("{}: parse json", path.display()))?;
    match value {
        Value::Array(items) => {
            for item in items {
                ingest_value(item, acc)?;
            }
        }
        obj @ Value::Object(_) => ingest_value(obj, acc)?,
        _ => {
            tracing::warn!(file = %path.display(), "hermes: top-level JSON is not object/array; skipped")
        }
    }
    Ok(())
}

/// One JSON object is either a snapshot (has `messages`) or a lone record.
fn ingest_value(value: Value, acc: &mut SessionAccumulator) -> Result<()> {
    let is_snapshot = value.get("messages").map(|m| m.is_array()).unwrap_or(false);
    if is_snapshot {
        let snap: RawSnapshot = serde_json::from_value(value).context("parse snapshot object")?;
        let sid = snap.id.clone().unwrap_or_else(|| "(unknown)".to_string());
        {
            let session = acc.session_mut(&sid);
            merge_snapshot_meta(session, &snap);
        }
        for rec in snap.messages {
            // Snapshot messages inherit the snapshot's session id.
            acc.ingest_record_for(&sid, rec);
        }
    } else {
        let rec: RawRecord = serde_json::from_value(value).context("parse record object")?;
        acc.ingest_record(rec);
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Accumulator
// ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct SessionAccumulator {
    order: Vec<String>,
    by_id: HashMap<String, HermesSession>,
}

impl SessionAccumulator {
    fn session_mut(&mut self, id: &str) -> &mut HermesSession {
        if !self.by_id.contains_key(id) {
            self.order.push(id.to_string());
            self.by_id.insert(
                id.to_string(),
                HermesSession {
                    id: id.to_string(),
                    ..Default::default()
                },
            );
        }
        self.by_id.get_mut(id).unwrap()
    }

    fn ingest_record(&mut self, rec: RawRecord) {
        let sid = rec
            .session_id
            .clone()
            .unwrap_or_else(|| "(unknown)".to_string());
        self.ingest_record_for(&sid, rec);
    }

    fn ingest_record_for(&mut self, sid: &str, rec: RawRecord) {
        let session = self.session_mut(sid);
        merge_record_meta(session, &rec);

        // Effective role: explicit `role`, else nested `author.role`.
        let role = rec
            .role
            .clone()
            .or_else(|| rec.author.as_ref().and_then(|a| a.role.clone()));
        let content = rec.content.as_ref().and_then(value_to_text);
        let reasoning = rec.reasoning.clone().filter(|s| !s.is_empty());
        let has_payload = role.is_some()
            || content.is_some()
            || reasoning.is_some()
            || rec.tool_calls.is_some()
            || rec.tool_name.is_some();
        if !has_payload {
            // A pure metadata / session-header record — nothing to render.
            return;
        }

        // Drop rewound / compacted messages from the normal transcript.
        if value_falsey(rec.active.as_ref()) || value_truthy(rec.compacted.as_ref()) {
            return;
        }

        session.messages.push(HermesMessage {
            role: role.unwrap_or_else(|| "assistant".to_string()),
            content,
            reasoning,
            tool_name: rec.tool_name.clone().filter(|s| !s.is_empty()),
            tool_calls_pretty: rec.tool_calls.as_ref().and_then(pretty_json),
            model: rec.model.clone().filter(|s| !s.is_empty()),
            timestamp_ms: rec.timestamp.as_ref().and_then(value_to_ms),
        });
    }

    fn into_sessions(mut self) -> Vec<HermesSession> {
        self.order
            .iter()
            .filter_map(|id| self.by_id.remove(id))
            .collect()
    }
}

fn merge_record_meta(session: &mut HermesSession, rec: &RawRecord) {
    set_if_empty(&mut session.title, rec.title.clone());
    set_if_empty(&mut session.source, rec.source.clone());
    set_if_empty(&mut session.model, rec.model.clone());
    set_if_empty(&mut session.user_id, rec.user_id.clone());
    set_if_empty(
        &mut session.parent_session_id,
        rec.parent_session_id.clone(),
    );
    if session.started_at_ms.is_none() {
        session.started_at_ms = rec.started_at.as_ref().and_then(value_to_ms);
    }
}

fn merge_snapshot_meta(session: &mut HermesSession, snap: &RawSnapshot) {
    set_if_empty(&mut session.title, snap.title.clone());
    set_if_empty(&mut session.source, snap.source.clone());
    set_if_empty(&mut session.model, snap.model.clone());
    set_if_empty(&mut session.user_id, snap.user_id.clone());
    set_if_empty(
        &mut session.parent_session_id,
        snap.parent_session_id.clone(),
    );
    if session.started_at_ms.is_none() {
        session.started_at_ms = snap.started_at.as_ref().and_then(value_to_ms);
    }
}

fn set_if_empty(slot: &mut Option<String>, value: Option<String>) {
    if slot.is_none() {
        if let Some(v) = value.filter(|s| !s.is_empty()) {
            *slot = Some(v);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Value coercion helpers
// ─────────────────────────────────────────────────────────────────────

/// Coerce a message `content` value into plain text. Accepts a string, an
/// OpenAI-style array of content parts (`[{type,text}]` or `["…"]`), or an
/// object with a `text` field.
fn value_to_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        Value::Array(arr) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|p| match p {
                    Value::String(s) => Some(s.clone()),
                    Value::Object(o) => o.get("text").and_then(|t| t.as_str()).map(str::to_string),
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n\n"))
        }
        Value::Object(o) => o
            .get("text")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// Coerce a timestamp value to unix **milliseconds**. Accepts epoch seconds or
/// millis (as a JSON number or numeric string) and RFC 3339 strings. Hermes
/// stores epoch seconds as a float; anything ≥ 1e12 is treated as already-ms.
fn value_to_ms(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(secs_or_ms_to_ms),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp_millis())
            .or_else(|| s.parse::<f64>().ok().map(secs_or_ms_to_ms)),
        _ => None,
    }
}

fn secs_or_ms_to_ms(f: f64) -> i64 {
    if f.abs() >= 1e12 {
        f as i64
    } else {
        (f * 1000.0) as i64
    }
}

/// `1` / `true` → true. Missing → false.
fn value_truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or(false),
        Some(Value::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Explicitly falsey (`0` / `false`). Missing → false (absence isn't "rewound"
/// — a record with no `active` flag is treated as active).
fn value_falsey(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => !*b,
        Some(Value::Number(n)) => n.as_i64().map(|i| i == 0).unwrap_or(false),
        Some(Value::String(s)) => s == "0" || s.eq_ignore_ascii_case("false"),
        _ => false,
    }
}

/// Pretty-print a `tool_calls` value for the rendered transcript. Accepts a
/// JSON value or a JSON string (Hermes stores it as TEXT).
fn pretty_json(v: &Value) -> Option<String> {
    let value = match v {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or_else(|_| v.clone()),
        other => other.clone(),
    };
    if value.is_null() {
        return None;
    }
    serde_json::to_string_pretty(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secs_and_ms_detection() {
        assert_eq!(secs_or_ms_to_ms(1_790_000_001.0), 1_790_000_001_000);
        assert_eq!(secs_or_ms_to_ms(1_790_000_001_000.0), 1_790_000_001_000);
    }

    #[test]
    fn content_array_flattens() {
        let v: Value = serde_json::json!([{"type": "text", "text": "a"}, "b"]);
        assert_eq!(value_to_text(&v).as_deref(), Some("a\n\nb"));
    }

    #[test]
    fn author_role_alias_and_generic_ids() {
        let mut acc = SessionAccumulator::default();
        let line = r#"{"conversation_id":"c1","author":{"role":"user"},"text":"hi","created_at":1790000001.0}"#;
        parse_jsonl(line, Path::new("x.jsonl"), &mut acc).unwrap();
        let sessions = acc.into_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "c1");
        assert_eq!(sessions[0].messages.len(), 1);
        assert_eq!(sessions[0].messages[0].role, "user");
        assert_eq!(sessions[0].messages[0].content.as_deref(), Some("hi"));
    }

    #[test]
    fn rewound_and_compacted_dropped() {
        let mut acc = SessionAccumulator::default();
        let lines = [
            r#"{"session_id":"s","role":"user","content":"keep"}"#,
            r#"{"session_id":"s","role":"assistant","content":"rewound","active":0}"#,
            r#"{"session_id":"s","role":"assistant","content":"compacted","compacted":1}"#,
        ]
        .join("\n");
        parse_jsonl(&lines, Path::new("x.jsonl"), &mut acc).unwrap();
        let sessions = acc.into_sessions();
        assert_eq!(sessions[0].messages.len(), 1);
        assert_eq!(sessions[0].messages[0].content.as_deref(), Some("keep"));
    }

    #[test]
    fn snapshot_shape_parses() {
        let mut acc = SessionAccumulator::default();
        let json = r#"{"id":"s_snap","source":"cli","title":"T","messages":[
            {"role":"user","content":"q","timestamp":1790000001.0},
            {"role":"assistant","content":"a","timestamp":1790000002.0}
        ]}"#;
        parse_json(json, Path::new("s.json"), &mut acc).unwrap();
        let sessions = acc.into_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s_snap");
        assert_eq!(sessions[0].source.as_deref(), Some("cli"));
        assert_eq!(sessions[0].messages.len(), 2);
    }
}
