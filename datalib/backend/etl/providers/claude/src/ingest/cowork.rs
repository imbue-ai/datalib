//! Cowork tasks run in Anthropic's cloud: what identifies one, what
//! version it is held at, and how its log becomes a conversation in the
//! export shape, so render draws it the way claude.ai draws a task it has
//! turned into a chat. Pure functions; the download and the render both
//! call them.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::normalize::synthesize_message_text;

/// What `_source.via` says on a conversation made from a Cowork log.
pub const VIA: &str = "claude.ai/cowork";

/// The `cse_…` id a configured entry names: a bare `cse_…`, the
/// `session_…` spelling the desktop app keeps, or a paste-able
/// `https://claude.ai/cowork/<id>` URL. `None` for anything else.
pub fn session_id(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s
        .strip_prefix("https://claude.ai/cowork/")
        .or_else(|| s.strip_prefix("claude.ai/cowork/"))
        .unwrap_or(s);
    let s = s.split(['?', '#', '/']).next().unwrap_or("");
    let suffix = s
        .strip_prefix("cse_")
        .or_else(|| s.strip_prefix("session_"))?;
    (!suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_alphanumeric()))
        .then(|| format!("cse_{suffix}"))
}

/// Where claude.ai shows the task.
pub fn url_of(session_id: &str) -> String {
    format!("https://claude.ai/cowork/{session_id}")
}

/// The version a listed task is held at: its last event, which moves
/// only when the log grows, and its title, which a rename moves without
/// a new event. `updated_at` is not used: it moves on every change of
/// the task's connection or status.
pub fn version_of(session: &Value) -> Option<String> {
    let last = session
        .get("last_event_at")
        .or_else(|| session.get("updated_at"))
        .and_then(Value::as_str)?;
    let title = session.get("title").and_then(Value::as_str).unwrap_or("");
    Some(format!("{last} {title}"))
}

/// The session as stored: its `tags` are a set the API returns in no
/// fixed order.
pub fn canonical_session(session: &Value) -> Value {
    let mut out = session.clone();
    if let Some(tags) = out.get_mut("tags").and_then(Value::as_array_mut) {
        tags.sort_by_key(|t| t.to_string());
    }
    out
}

/// The key one event is stored under: its `event_id`, or its place in
/// the log when it has none.
pub fn event_key(session_id: &str, event: &Value) -> String {
    match event.get("event_id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => format!("{session_id}#{}", sequence_of(event).unwrap_or(-1)),
    }
}

/// An event's place in its log. The API sends it as a string.
pub fn sequence_of(event: &Value) -> Option<i64> {
    match event.get("sequence_num")? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// Where a conversation made from a log came from, for render.
pub struct Origin<'a> {
    pub account_uuid: Option<&'a str>,
    pub org_uuid: &'a str,
    pub org_name: Option<&'a str>,
}

/// One Cowork task as a conversation in the export shape. Each turn the
/// person typed is a `human` message; what Claude did until the next one
/// — thinking, text, each tool call and its result — is one `assistant`
/// message, the way claude.ai keeps a task it has made a chat. What the
/// app attaches to a person's message (`<system-reminder>` blocks), the
/// harness's own events and a subagent's inner turns are left out; the
/// raw store keeps them.
pub fn to_export_shape(session: &Value, events: &[Value], origin: &Origin<'_>) -> Value {
    let mut ordered: Vec<&Value> = events.iter().collect();
    ordered.sort_by_key(|e| sequence_of(e).unwrap_or(i64::MAX));

    let mut turns = Turns::default();
    for e in ordered {
        let Some(p) = e.get("payload") else { continue };
        if p.get("parent_tool_use_id").is_some_and(|v| !v.is_null()) {
            continue;
        }
        let at = p
            .get("timestamp")
            .or_else(|| e.get("created_at"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match p.get("type").and_then(Value::as_str) {
            Some("user") => turns.user(p, &at),
            Some("assistant") => turns.assistant(p, &at),
            Some("result") => turns.close(),
            _ => {}
        }
    }
    turns.close();

    let str_of = |k: &str| session.get(k).cloned().unwrap_or(Value::Null);
    let id = session.get("id").and_then(Value::as_str).unwrap_or("");
    let mut conv = json!({
        "uuid": id,
        "name": str_of("title"),
        "summary": "",
        "created_at": str_of("created_at"),
        "updated_at": session.get("last_event_at").cloned().unwrap_or_else(|| str_of("updated_at")),
        "model": session.get("config").and_then(|c| c.get("model")).cloned().unwrap_or(Value::Null),
        "current_leaf_message_uuid": turns.last_uuid.clone().map(Value::String).unwrap_or(Value::Null),
        "chat_messages": turns.messages,
        "_source": {"via": VIA, "org_uuid": origin.org_uuid},
    });
    if let Some(name) = origin.org_name {
        conv["_source"]["org_name"] = json!(name);
    }
    if let Some(acct) = origin.account_uuid {
        conv["account"] = json!({ "uuid": acct });
    }
    conv
}

#[derive(Default)]
struct Turns {
    messages: Vec<Value>,
    open: Option<Map<String, Value>>,
    tool_names: HashMap<String, Value>,
    last_uuid: Option<String>,
}

impl Turns {
    fn user(&mut self, p: &Value, at: &str) {
        let blocks = content_of(p);
        let results: Vec<&Value> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .collect();
        for r in results {
            let tool_use_id = r.get("tool_use_id").cloned().unwrap_or(Value::Null);
            let name = tool_use_id
                .as_str()
                .and_then(|id| self.tool_names.get(id))
                .cloned()
                .unwrap_or(Value::Null);
            let block = json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "name": name,
                "content": result_content(r.get("content")),
                "is_error": r.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                "start_timestamp": at,
                "stop_timestamp": at,
            });
            self.assistant_turn(p, at).push(block);
        }
        let typed: Vec<Value> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
            .map(|t| {
                json!({"type": "text", "text": t, "citations": [],
                       "start_timestamp": at, "stop_timestamp": at})
            })
            .collect();
        let files = attached(p);
        if typed.is_empty() && files.is_empty() {
            return;
        }
        self.close();
        let uuid = uuid_of(p, self.messages.len());
        let message = json!({
            "uuid": uuid,
            "sender": "human",
            "text": synthesize_message_text(&typed),
            "content": typed,
            "created_at": at,
            "updated_at": at,
            "attachments": [],
            "files": files,
            "parent_message_uuid": self.last_uuid,
        });
        self.messages.push(message);
        self.last_uuid = Some(uuid);
    }

    fn assistant(&mut self, p: &Value, at: &str) {
        let mut out = Vec::new();
        for b in content_of(p) {
            match b.get("type").and_then(Value::as_str) {
                Some("thinking") => {
                    let text = b.get("thinking").and_then(Value::as_str).unwrap_or("");
                    // Thinking sent as its signature alone has nothing to show.
                    if !text.is_empty() {
                        out.push(json!({"type": "thinking", "thinking": text,
                                        "start_timestamp": at, "stop_timestamp": at}));
                    }
                }
                Some("text") => out.push(json!({
                    "type": "text",
                    "text": b.get("text").cloned().unwrap_or(json!("")),
                    "citations": [],
                    "start_timestamp": at,
                    "stop_timestamp": at,
                })),
                Some("tool_use") => {
                    let id = b.get("id").cloned().unwrap_or(Value::Null);
                    let name = b.get("name").cloned().unwrap_or(Value::Null);
                    if let Some(id) = id.as_str() {
                        self.tool_names.insert(id.to_string(), name.clone());
                    }
                    out.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": b.get("input").cloned().unwrap_or(json!({})),
                        "start_timestamp": at,
                        "stop_timestamp": at,
                    }));
                }
                _ => {}
            }
        }
        let turn = self.assistant_turn(p, at);
        turn.extend(out);
    }

    /// The assistant message the blocks of this turn go into, opened at
    /// the first of them.
    fn assistant_turn(&mut self, p: &Value, at: &str) -> &mut Vec<Value> {
        if self.open.is_none() {
            let uuid = uuid_of(p, self.messages.len());
            let mut m = Map::new();
            m.insert("uuid".into(), json!(uuid));
            m.insert("sender".into(), json!("assistant"));
            m.insert("created_at".into(), json!(at));
            m.insert("content".into(), json!([]));
            m.insert("attachments".into(), json!([]));
            m.insert("files".into(), json!([]));
            m.insert("parent_message_uuid".into(), json!(self.last_uuid));
            self.open = Some(m);
        }
        let m = self.open.as_mut().expect("opened above");
        m.insert("updated_at".into(), json!(at));
        m.get_mut("content")
            .and_then(Value::as_array_mut)
            .expect("content is an array")
    }

    /// A reply is dated when it ended, every block in it too, as
    /// claude.ai dates a task it has made a chat: render then lists the
    /// steps before the answer.
    fn close(&mut self) {
        let Some(mut m) = self.open.take() else {
            return;
        };
        let ended = m.get("updated_at").cloned().unwrap_or(Value::Null);
        let mut blocks = m
            .get("content")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if blocks.is_empty() {
            return;
        }
        for b in &mut blocks {
            b["start_timestamp"] = ended.clone();
            b["stop_timestamp"] = ended.clone();
        }
        m.insert("created_at".into(), ended);
        m.insert("text".into(), json!(synthesize_message_text(&blocks)));
        m.insert("content".into(), Value::Array(blocks));
        let uuid = m.get("uuid").and_then(Value::as_str).map(String::from);
        self.messages.push(Value::Object(m));
        self.last_uuid = uuid;
    }
}

/// Every file the person attached to the task, each `file_uuid` once,
/// as the chat-shaped file objects [`to_export_shape`] gives its
/// messages: what the attachment loop fetches.
pub fn files_of(events: &[Value]) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    events
        .iter()
        .filter_map(|e| e.get("payload"))
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("user"))
        .filter(|p| p.get("parent_tool_use_id").is_none_or(Value::is_null))
        .flat_map(attached)
        .filter(|f| {
            f.get("file_uuid")
                .and_then(Value::as_str)
                .is_some_and(|id| seen.insert(id.to_string()))
        })
        .collect()
}

/// A person's message names its files beside its text, in
/// `file_attachments` (`file_uuid`, `file_name`, `file_size`,
/// `is_image`), as a chat's message names them in `files`.
fn attached(p: &Value) -> Vec<Value> {
    let Some(list) = p.get("file_attachments").and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter(|a| a.get("file_uuid").and_then(Value::as_str).is_some())
        .map(|a| {
            let image = a.get("is_image").and_then(Value::as_bool).unwrap_or(false);
            json!({
                "file_uuid": a["file_uuid"],
                "file_name": a.get("file_name").cloned().unwrap_or(Value::Null),
                "size_bytes": a.get("file_size").cloned().unwrap_or(Value::Null),
                "file_kind": if image { "image" } else { "document" },
            })
        })
        .collect()
}

fn content_of(p: &Value) -> Vec<Value> {
    match p.get("message").and_then(|m| m.get("content")) {
        Some(Value::Array(blocks)) => blocks.clone(),
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        _ => Vec::new(),
    }
}

/// A tool result's content as the export keeps it: a list of blocks.
fn result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::Array(blocks)) => Value::Array(blocks.clone()),
        Some(Value::String(s)) => json!([{"type": "text", "text": s}]),
        _ => json!([]),
    }
}

/// An event's own uuid, or one made from its place when it has none.
fn uuid_of(p: &Value, index: usize) -> String {
    p.get("uuid")
        .and_then(Value::as_str)
        .map(String::from)
        .unwrap_or_else(|| format!("cowork-message-{index}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: i64, payload: Value) -> Value {
        json!({"event_id": format!("ev{seq}"), "sequence_num": seq.to_string(), "payload": payload})
    }

    fn sample_log() -> Vec<Value> {
        vec![
            event(1, json!({"type": "control_request", "uuid": "c1"})),
            event(
                2,
                json!({"type": "user", "uuid": "u1", "timestamp": "2369-04-15T10:00:00Z",
                "file_attachments": [{"file_uuid": "f1", "file_name": "bridge.png",
                                      "file_size": 10, "is_image": true}],
                "message": {"role": "user", "content": [
                    {"type": "text", "text": "<system-reminder>The user's timezone is Earth</system-reminder>"},
                    {"type": "text", "text": "List the senior staff."}]}}),
            ),
            event(
                3,
                json!({"type": "system", "subtype": "init", "uuid": "s1"}),
            ),
            event(
                4,
                json!({"type": "assistant", "uuid": "a1", "timestamp": "2369-04-15T10:00:01Z",
                "message": {"content": [{"type": "thinking", "thinking": "", "signature": "x"}]}}),
            ),
            event(
                5,
                json!({"type": "assistant", "uuid": "a2", "timestamp": "2369-04-15T10:00:02Z",
                "message": {"content": [{"type": "tool_use", "id": "toolu_1", "name": "Read",
                                          "input": {"file_path": "crew.txt"}}]}}),
            ),
            event(
                6,
                json!({"type": "user", "uuid": "u2", "timestamp": "2369-04-15T10:00:03Z",
                "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1",
                                          "content": "Picard, Riker, Data"}]}}),
            ),
            event(
                7,
                json!({"type": "assistant", "uuid": "a3", "parent_tool_use_id": "toolu_9",
                "message": {"content": [{"type": "text", "text": "a subagent's aside"}]}}),
            ),
            event(
                8,
                json!({"type": "assistant", "uuid": "a4", "timestamp": "2369-04-15T10:00:04Z",
                "message": {"content": [{"type": "text", "text": "Three officers."}]}}),
            ),
            event(
                9,
                json!({"type": "result", "subtype": "success", "uuid": "r1"}),
            ),
            event(
                10,
                json!({"type": "user", "uuid": "u3", "timestamp": "2369-04-15T10:01:00Z",
                "message": {"content": "Add Worf."}}),
            ),
        ]
    }

    fn session() -> Value {
        json!({"id": "cse_01Abc", "title": "Senior staff", "created_at": "2369-04-15T09:59:59Z",
               "last_event_at": "2369-04-15T10:01:00Z", "config": {"model": "claude-opus"}})
    }

    /// A log becomes the turns a person reads: what they typed, then one
    /// reply holding every step Claude took, the tool's result beside its
    /// call. The app's own context, the harness's events, a subagent's
    /// inner turns and a signature-only thinking block are not drawn.
    #[test]
    fn a_log_becomes_human_and_assistant_turns() {
        let origin = Origin {
            account_uuid: Some("acct"),
            org_uuid: "org",
            org_name: Some("Enterprise"),
        };
        // Out of order on purpose: the log is read in its own order.
        let mut log = sample_log();
        log.reverse();
        let conv = to_export_shape(&session(), &log, &origin);
        let messages = conv["chat_messages"].as_array().unwrap();
        let senders: Vec<&str> = messages
            .iter()
            .map(|m| m["sender"].as_str().unwrap())
            .collect();
        assert_eq!(senders, ["human", "assistant", "human"]);
        assert_eq!(messages[0]["text"], "List the senior staff.");
        assert_eq!(messages[0]["uuid"], "u1");
        assert_eq!(messages[0]["files"][0]["file_uuid"], "f1");
        assert_eq!(messages[0]["files"][0]["file_kind"], "image");
        assert_eq!(files_of(&log).len(), 1);
        let blocks: Vec<&str> = messages[1]["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect();
        assert_eq!(blocks, ["tool_use", "tool_result", "text"]);
        assert_eq!(messages[1]["content"][1]["name"], "Read");
        assert_eq!(
            messages[1]["content"][1]["content"][0]["text"],
            "Picard, Riker, Data"
        );
        assert_eq!(messages[1]["text"], "Three officers.");
        assert_eq!(messages[1]["parent_message_uuid"], "u1");
        assert_eq!(messages[2]["text"], "Add Worf.");
        assert_eq!(messages[2]["parent_message_uuid"], messages[1]["uuid"]);
        assert_eq!(conv["uuid"], "cse_01Abc");
        assert_eq!(conv["name"], "Senior staff");
        assert_eq!(conv["updated_at"], "2369-04-15T10:01:00Z");
        assert_eq!(conv["_source"]["via"], VIA);
        assert_eq!(conv["_source"]["org_name"], "Enterprise");
        assert_eq!(conv["account"]["uuid"], "acct");
        assert_eq!(conv["current_leaf_message_uuid"], "u3");
    }

    #[test]
    fn a_session_is_named_three_ways() {
        let want = Some("cse_01FxAnSL".to_string());
        assert_eq!(session_id("cse_01FxAnSL"), want);
        assert_eq!(session_id("session_01FxAnSL"), want);
        assert_eq!(
            session_id("https://claude.ai/cowork/cse_01FxAnSL?x=1"),
            want
        );
        assert_eq!(session_id(" cse_01FxAnSL/ "), want);
        assert_eq!(session_id("https://claude.ai/chat/0000"), None);
        assert_eq!(session_id("cse_"), None);
        assert_eq!(session_id("cse_a-b"), None);
    }

    /// A rename moves the version without a new event; a change of the
    /// task's status alone (`updated_at`) does not.
    #[test]
    fn the_version_follows_the_log_and_the_title() {
        let s = session();
        let mut renamed = s.clone();
        renamed["title"] = json!("Bridge crew");
        let mut status_only = s.clone();
        status_only["updated_at"] = json!("2369-04-16T00:00:00Z");
        assert_ne!(version_of(&s), version_of(&renamed));
        assert_eq!(version_of(&s), version_of(&status_only));
    }

    #[test]
    fn tags_are_stored_in_one_order() {
        let a = canonical_session(&json!({"tags": ["b", "a"]}));
        let b = canonical_session(&json!({"tags": ["a", "b"]}));
        assert_eq!(a, b);
    }

    #[test]
    fn an_event_without_an_id_is_keyed_by_its_place() {
        assert_eq!(event_key("cse_1", &json!({"event_id": "e9"})), "e9");
        assert_eq!(event_key("cse_1", &json!({"sequence_num": "4"})), "cse_1#4");
    }
}
