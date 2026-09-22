//! One rollout file → its lines and what the file says about itself. A
//! rollout is JSONL: every line is `{timestamp, type, payload}`, where
//! `type` is one of `session_meta` (once, first), `turn_context` (once
//! per turn), `response_item` (the model-visible history: messages,
//! tool calls and their outputs, reasoning), `event_msg` (what the UI
//! was told) and `compacted`. Every parsable line becomes a row; what
//! the file says about the thread as a whole is read off `session_meta`
//! and the turn contexts and folds into the transcript row. Anything
//! unparsable is counted, not fatal.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ParsedRollout {
    pub thread_id: String,
    pub meta: RolloutMeta,
    pub records: Vec<ParsedLine>,
    pub stats: ParseStats,
}

/// The `records.id` for a line: Codex gives a line no id of its own,
/// and a rollout only grows, so its number is the key.
pub fn record_id(thread_id: &str, line_no: i64) -> String {
    format!("{thread_id}#{line_no}")
}

/// What the file says about the thread as a whole; stored as the
/// transcript row's payload.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RolloutMeta {
    pub thread_id: String,
    /// The thread this one was spawned from, when Codex spawned it as a
    /// sub-agent; the fork parent is `forked_from_id`.
    pub parent_thread_id: Option<String>,
    pub forked_from_id: Option<String>,
    pub rel_path: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
    pub git_origin_url: Option<String>,
    /// Which Codex wrote it: `codex_cli_rs`, the VS Code extension, …
    pub originator: Option<String>,
    pub cli_version: Option<String>,
    /// `cli`, `exec`, `vscode`, or an object naming a sub-agent's kind.
    pub source: Option<Value>,
    pub model_provider: Option<String>,
    /// Every model a turn ran on, from the turn contexts.
    pub models: BTreeSet<String>,
    pub title: Option<String>,
    /// Where `title` came from: `agent` (a sub-agent's nickname) or
    /// `prompt` (the first thing the person typed).
    pub title_source: Option<&'static str>,
    pub first_prompt: Option<String>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub turns: usize,
    /// Lines by `type`.
    pub record_counts: BTreeMap<String, usize>,
    /// `response_item` lines by their payload's `type`.
    pub item_counts: BTreeMap<String, usize>,
}

/// One line: its 1-based number in the file and the line as written.
#[derive(Debug, Clone)]
pub struct ParsedLine {
    pub line_no: i64,
    pub raw: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParseStats {
    pub lines: usize,
    pub records: usize,
    pub malformed: usize,
}

/// `None` when no line is a `session_meta` — an empty or foreign file.
pub fn parse_rollout(text: &str, rel_path: &str) -> Option<ParsedRollout> {
    let mut meta = RolloutMeta {
        rel_path: rel_path.to_string(),
        ..Default::default()
    };
    let mut records = Vec::new();
    let mut stats = ParseStats::default();
    let mut thread_id: Option<String> = None;

    for (i, line) in text.lines().enumerate() {
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
        if let Some(ts) = str_field(&v, "timestamp") {
            if meta.started_at.as_deref().is_none_or(|s| ts.as_str() < s) {
                meta.started_at = Some(ts.clone());
            }
            if meta.updated_at.as_deref().is_none_or(|s| ts.as_str() > s) {
                meta.updated_at = Some(ts);
            }
        }
        let payload = v.get("payload").unwrap_or(&Value::Null);
        match record_type {
            "session_meta" => {
                if thread_id.is_none() {
                    thread_id = str_field(payload, "id");
                    note_session_meta(&mut meta, payload);
                }
            }
            "turn_context" => {
                meta.turns += 1;
                if let Some(m) = str_field(payload, "model") {
                    meta.models.insert(m);
                }
                if meta.cwd.is_none() {
                    meta.cwd = str_field(payload, "cwd");
                }
            }
            "response_item" => {
                let item_type = str_field(payload, "type").unwrap_or_default();
                *meta.item_counts.entry(item_type).or_default() += 1;
            }
            "event_msg"
                if meta.first_prompt.is_none()
                    && str_field(payload, "type").as_deref() == Some("user_message") =>
            {
                meta.first_prompt = str_field(payload, "message").map(|m| first_line(&m));
            }
            _ => {}
        }
        records.push(ParsedLine {
            line_no: i as i64 + 1,
            raw: v,
        });
        stats.records += 1;
    }

    let thread_id = thread_id?;
    meta.thread_id = thread_id.clone();
    (meta.title, meta.title_source) = match (&meta.agent_nickname, &meta.first_prompt) {
        (Some(n), _) => (Some(n.clone()), Some("agent")),
        (None, Some(p)) => (Some(p.clone()), Some("prompt")),
        (None, None) => (None, None),
    };
    Some(ParsedRollout {
        thread_id,
        meta,
        records,
        stats,
    })
}

fn note_session_meta(meta: &mut RolloutMeta, p: &Value) {
    meta.cwd = str_field(p, "cwd");
    meta.originator = str_field(p, "originator");
    meta.cli_version = str_field(p, "cli_version");
    meta.model_provider = str_field(p, "model_provider");
    meta.source = p.get("source").filter(|s| !s.is_null()).cloned();
    meta.forked_from_id = str_field(p, "forked_from_id");
    meta.agent_nickname = str_field(p, "agent_nickname");
    meta.agent_role = str_field(p, "agent_role").or_else(|| str_field(p, "agent_type"));
    // The parent is a field of its own on a newer Codex, and inside the
    // `source` object on the one that introduced sub-agents.
    meta.parent_thread_id = str_field(p, "parent_thread_id").or_else(|| {
        p.get("source")
            .and_then(|s| s.get("subagent"))
            .and_then(|s| s.get("thread_spawn"))
            .and_then(|s| str_field(s, "parent_thread_id"))
    });
    if let Some(git) = p.get("git") {
        meta.git_branch = str_field(git, "branch");
        meta.git_commit = str_field(git, "commit_hash");
        meta.git_origin_url = str_field(git, "repository_url");
    }
    // The session's own stamp is when it began; the first line's is
    // when it was written, which can be later.
    if let Some(ts) = str_field(p, "timestamp") {
        if meta.started_at.as_deref().is_none_or(|s| ts.as_str() < s) {
            meta.started_at = Some(ts);
        }
    }
}

/// The first line of what the person typed, for a title.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate_chars(line, 100)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

pub fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(ts: &str, kind: &str, payload: Value) -> String {
        json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
    }

    fn session_meta(id: &str, extra: Value) -> Value {
        let mut p = json!({
            "id": id, "timestamp": "2364-04-11T10:00:00.000Z",
            "cwd": "/Users/picard/src/enterprise", "originator": "codex_cli_rs",
            "cli_version": "0.115.0", "source": "cli", "model_provider": "openai",
            "base_instructions": {"text": "You are Codex."},
            "git": {"commit_hash": "abc", "branch": "main", "repository_url": "https://example/e.git"}
        });
        if let Value::Object(m) = extra {
            for (k, v) in m {
                p[k] = v;
            }
        }
        p
    }

    #[test]
    fn every_line_is_a_row_and_the_meta_folds() {
        let text = [
            line("2364-04-11T10:00:01.000Z", "session_meta", session_meta("t1", json!({}))),
            line("2364-04-11T10:00:01.100Z", "turn_context", json!({"turn_id": "u1", "cwd": "/Users/picard/src/enterprise", "model": "gpt-5.3-codex"})),
            line("2364-04-11T10:00:01.200Z", "response_item", json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Realign the deflector dish"}]})),
            line("2364-04-11T10:00:01.200Z", "event_msg", json!({"type": "user_message", "message": "Realign the deflector dish\nplease", "images": []})),
            line("2364-04-11T10:00:05.000Z", "response_item", json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Aye."}]})),
            "not json at all".to_string(),
            line("2364-04-11T10:00:06.000Z", "event_msg", json!({"type": "task_complete", "turn_id": "u1"})),
        ]
        .join("\n");
        let r = parse_rollout(&text, "sessions/2364/04/11/rollout-t1.jsonl").unwrap();
        assert_eq!(r.thread_id, "t1");
        assert_eq!(r.records.len(), 6);
        assert_eq!(
            r.records.iter().map(|l| l.line_no).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 7],
            "line numbers are physical, so a malformed line keeps its slot"
        );
        assert_eq!(
            r.stats,
            ParseStats {
                lines: 7,
                records: 6,
                malformed: 1
            }
        );
        assert_eq!(r.meta.title.as_deref(), Some("Realign the deflector dish"));
        assert_eq!(r.meta.title_source, Some("prompt"));
        assert_eq!(r.meta.cwd.as_deref(), Some("/Users/picard/src/enterprise"));
        assert_eq!(r.meta.git_branch.as_deref(), Some("main"));
        assert_eq!(r.meta.cli_version.as_deref(), Some("0.115.0"));
        assert_eq!(r.meta.source, Some(json!("cli")));
        assert_eq!(r.meta.turns, 1);
        assert!(r.meta.models.contains("gpt-5.3-codex"));
        assert_eq!(
            r.meta.started_at.as_deref(),
            Some("2364-04-11T10:00:00.000Z"),
            "the session's own stamp, earlier than its first line"
        );
        assert_eq!(
            r.meta.updated_at.as_deref(),
            Some("2364-04-11T10:00:06.000Z")
        );
        assert_eq!(r.meta.record_counts["event_msg"], 2);
        assert_eq!(r.meta.item_counts["message"], 2);
        assert_eq!(record_id("t1", 7), "t1#7");
    }

    /// A sub-agent thread names its parent either as its own field or
    /// inside `source`, depending on the Codex that wrote it; its
    /// nickname is its title.
    #[test]
    fn a_subagent_names_its_parent_either_way() {
        let new = line(
            "2364-04-11T10:00:01.000Z",
            "session_meta",
            session_meta(
                "t2",
                json!({"parent_thread_id": "t1", "agent_nickname": "Data", "agent_role": "explorer"}),
            ),
        );
        let r = parse_rollout(&new, "x.jsonl").unwrap();
        assert_eq!(r.meta.parent_thread_id.as_deref(), Some("t1"));
        assert_eq!(r.meta.title.as_deref(), Some("Data"));
        assert_eq!(r.meta.title_source, Some("agent"));
        assert_eq!(r.meta.agent_role.as_deref(), Some("explorer"));

        let old = line(
            "2364-04-11T10:00:01.000Z",
            "session_meta",
            session_meta(
                "t3",
                json!({"source": {"subagent": {"thread_spawn": {"parent_thread_id": "t1", "depth": 1}}}}),
            ),
        );
        let r = parse_rollout(&old, "x.jsonl").unwrap();
        assert_eq!(r.meta.parent_thread_id.as_deref(), Some("t1"));
        assert!(r.meta.source.is_some());
    }

    #[test]
    fn a_file_with_no_session_meta_is_not_a_rollout() {
        assert!(parse_rollout("", "x.jsonl").is_none());
        assert!(parse_rollout(
            "{\"session_id\":\"t1\",\"ts\":1,\"text\":\"hi\"}",
            "history.jsonl"
        )
        .is_none());
    }

    #[test]
    fn a_long_first_prompt_is_cut_for_the_title() {
        let long = "x".repeat(150);
        let text = [
            line(
                "2364-04-11T10:00:01.000Z",
                "session_meta",
                session_meta("t1", json!({})),
            ),
            line(
                "2364-04-11T10:00:01.200Z",
                "event_msg",
                json!({"type": "user_message", "message": long}),
            ),
        ]
        .join("\n");
        let r = parse_rollout(&text, "x.jsonl").unwrap();
        let t = r.meta.title.unwrap();
        assert_eq!(t.chars().count(), 100);
        assert!(t.ends_with('…'));
    }
}
