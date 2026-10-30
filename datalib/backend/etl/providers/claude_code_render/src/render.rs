//! One document per transcript, through the shared chat renderer. A
//! session reads as what was said, with each run of tool calls and
//! results folded into one collapsed block; a subagent's transcript is
//! its own document.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::normalize::iso_to_ms;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    AuthorKind, ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_chat_common::RenderTarget;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Inputs, RawRange};
use datalib_etl_render::pages::PageSize;
use datalib_handle::Handle;
use serde_json::Value;
use sqlx::SqlitePool;

use datalib_etl_claude_code::ingest::db_path_for;
use datalib_etl_claude_code::ingest::parse::{human_text, transcript_id};
use datalib_schema::providers::Provider;

use datalib_etl_agent_sessions_render::{
    clamp, details, fenced, item, json_block, project_of, render_paged, str_of, PagedOutcome,
    SessionRender,
};

use crate::ids;

/// v2: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout).
/// v4: the account's own messages are "Me", as every chat source writes
///     them.
/// v5: an answer's author is its model's chip (`ai_model:` handle).
/// v6: what the harness writes in the person's place (notices, reminders,
///     a subagent's brief) is a Harness Message, not "Me".
pub const RENDER_VERSION: u32 = 6;

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::ClaudeCode,
        source_label: "Claude Code".to_string(),
        chat_kind: "Claude Code Session".to_string(),
        // Per-item kind is always set via `kind_label`; nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Claude Code Reaction".to_string(),
        chat_entity_kind: ids::KIND_SESSION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

/// Renders the transcripts this run has to, a page at a time;
/// `on_buckets` hears each page's buckets before the next is read.
#[allow(clippy::too_many_arguments)]
pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    on_buckets: &mut dyn FnMut(&[Bucket]) -> Result<()>,
    range: RawRange<'_>,
    size: PageSize,
    max_tool_result_bytes: usize,
) -> Result<PagedOutcome> {
    render_paged(
        &ClaudeCode {
            source_id,
            max_tool_result_bytes,
        },
        &db_path_for(raw_dir),
        range,
        size,
        RenderTarget {
            out_root,
            source_id,
            progress,
            on_doc_complete,
        },
        on_buckets,
    )
}

struct ClaudeCode<'a> {
    source_id: &'a str,
    max_tool_result_bytes: usize,
}

impl SessionRender for ClaudeCode<'_> {
    type Record = (String, Value);

    fn profile(&self) -> RenderProfile {
        profile()
    }

    fn chat_uuid(&self, transcript_id: &str) -> String {
        chat_uuid_of(self.source_id, transcript_id)
    }

    async fn load_records(&self, pool: &SqlitePool, ids: &str) -> Result<Vec<(String, Value)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, json(payload) FROM records \
             WHERE transcript_id IN (SELECT value FROM json_each(?1)) AND payload IS NOT NULL",
        )
        .bind(ids)
        .fetch_all(pool)
        .await
        .context("select a page of records")?;
        rows.into_iter()
            .map(|(id, payload)| Ok((id, serde_json::from_str(&payload)?)))
            .collect::<Result<_>>()
            .context("read a page of records")
    }

    fn build_chats(
        &self,
        page: &[(String, Value)],
        records: &[(String, Value)],
        titles: &HashMap<String, String>,
    ) -> Vec<NormalizedChat> {
        build_chats(
            self.source_id,
            page,
            records,
            titles,
            self.max_tool_result_bytes,
        )
    }
}

/// The document id for a raw transcript id, with no row in hand.
fn chat_uuid_of(source_id: &str, tid: &str) -> String {
    let (session_id, agent_id) = match tid.split_once('#') {
        Some((s, a)) => (s, Some(a)),
        None => (tid, None),
    };
    ids::transcript(source_id, session_id, agent_id).uuid
}

fn build_chats(
    source_id: &str,
    transcripts: &[(String, Value)],
    records: &[(String, Value)],
    titles: &HashMap<String, String>,
    max_tool_result_bytes: usize,
) -> Vec<NormalizedChat> {
    let mut by_transcript: BTreeMap<String, Vec<(&str, &Value)>> = BTreeMap::new();
    for (row_id, v) in records {
        let session = str_of(v, "sessionId").unwrap_or("");
        let key = transcript_id(session, str_of(v, "agentId"));
        by_transcript.entry(key).or_default().push((row_id, v));
    }

    let mut chats = Vec::with_capacity(transcripts.len());
    for (tid, meta) in transcripts {
        let mut rows = by_transcript.remove(tid).unwrap_or_default();
        rows.sort_by(|a, b| {
            (str_of(a.1, "timestamp").unwrap_or(""), a.0)
                .cmp(&(str_of(b.1, "timestamp").unwrap_or(""), b.0))
        });
        let parent_title = str_of(meta, "agent_id")
            .and_then(|_| str_of(meta, "session_id"))
            .and_then(|s| titles.get(s).map(String::as_str));
        chats.push(build_chat(
            source_id,
            tid,
            meta,
            &rows,
            parent_title,
            max_tool_result_bytes,
        ));
    }
    chats
}

fn build_chat(
    source_id: &str,
    tid: &str,
    meta: &Value,
    rows: &[(&str, &Value)],
    parent_title: Option<&str>,
    max_tool_result_bytes: usize,
) -> NormalizedChat {
    let session_id = str_of(meta, "session_id").unwrap_or(tid);
    let agent_id = str_of(meta, "agent_id");
    let inputs = Inputs::default();
    inputs.read("transcripts", tid);

    // A tool result names only the tool-use id it answers; the tool's
    // name is on the call.
    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    for (_, v) in rows {
        for b in blocks_of(v) {
            if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let (Some(id), Some(name)) = (str_of(b, "id"), str_of(b, "name")) {
                    tool_names.insert(id, name);
                }
            }
        }
    }

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    let mut last_ms: Option<i64> = str_of(meta, "started_at").and_then(iso_to_ms);
    for (row_id, v) in rows {
        inputs.read("records", row_id);
        let Some(uuid) = str_of(v, "uuid") else {
            continue;
        };
        let ms = str_of(v, "timestamp")
            .and_then(iso_to_ms)
            .or_else(|| last_ms.map(|p| p + 1));
        last_ms = ms.or(last_ms);
        match str_of(v, "type") {
            Some("user") => user_items(
                source_id,
                uuid,
                v,
                ms,
                &tool_names,
                max_tool_result_bytes,
                &mut items,
            ),
            Some("assistant") => {
                assistant_items(source_id, uuid, v, ms, max_tool_result_bytes, &mut items)
            }
            Some("system") => {
                if let Some(it) = system_item(source_id, uuid, v, ms) {
                    items.push(it);
                }
            }
            _ => {}
        }
    }
    items.sort_by_key(|i| i.date_ms);

    let own_title = str_of(meta, "title").unwrap_or("(untitled)").to_string();
    let display = match (agent_id, parent_title) {
        (Some(_), Some(parent)) => format!("{own_title} — subagent of {parent}"),
        (Some(_), None) => format!("{own_title} — subagent"),
        (None, _) => own_title.clone(),
    };
    let id = ids::transcript(source_id, session_id, agent_id);
    debug_assert_eq!(id.uuid, chat_uuid_of(source_id, tid));
    NormalizedChat {
        named_after: Vec::new(),
        id: tid.to_string(),
        chat_uuid: id.uuid.clone(),
        display,
        title: Some(own_title),
        author: None,
        account: None,
        project: str_of(meta, "cwd").map(project_of),
        external_id: Some(id.natural_key.clone()),
        source_url: str_of(meta, "cloud_session_id").map(|c| format!("https://claude.ai/code/{c}")),
        upstream_account: None,
        org_uuid: str_of(meta, "org_uuid").map(str::to_string),
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: id.uuid,
            // A subagent's transcript is its own kind of document; the
            // profile's `chat_entity_kind` names the session's.
            source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
            items,
        }],
        contacts: Vec::new(),
        inputs: inputs.declared(),
    }
}

fn user_items(
    source_id: &str,
    uuid: &str,
    v: &Value,
    ms: Option<i64>,
    tool_names: &HashMap<&str, &str>,
    max_bytes: usize,
    out: &mut Vec<NormalizedChatItem>,
) {
    let content = v.get("message").and_then(|m| m.get("content"));
    let mut n = 0usize;
    for b in content.map(blocks_of_content).unwrap_or_default() {
        if b.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let tool_use_id = str_of(b, "tool_use_id").unwrap_or("");
        let name = tool_names.get(tool_use_id).copied().unwrap_or("tool");
        let id = ids::tool_result(source_id, uuid, tool_use_id, ms.map(|m| m + n as i64));
        let is_err = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let summary = if is_err {
            format!("Tool result: {name} (error)")
        } else {
            format!("Tool result: {name}")
        };
        let body = fenced(&clamp(&tool_result_text(b.get("content")), max_bytes));
        out.push(item(
            id,
            name.to_string(),
            ms.map(|m| m + n as i64),
            details(&summary, &body),
            "Tool Result",
            true,
        ));
        n += 1;
    }
    let Some(content) = content else {
        return;
    };
    let (reminders, mut text) = split_reminders(&human_text(content).unwrap_or_default());
    if !reminders.is_empty() {
        out.push(NormalizedChatItem {
            author_kind: AuthorKind::Named,
            ..item(
                ids::reminder(source_id, uuid, ms.map(|m| m + n as i64)),
                "Claude Code".to_string(),
                ms.map(|m| m + n as i64),
                reminders.join("\n\n"),
                "Harness Message",
                true,
            )
        });
    }
    let images = content
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("image"))
                .count()
        })
        .unwrap_or(0);
    if images > 0 {
        text = format!("{text}\n\n*[{images} image(s) not shown]*")
            .trim()
            .to_string();
    }
    if text.trim().is_empty() {
        return;
    }
    let (label, author, author_kind, aside) = if written_by_the_harness(v, &text) {
        ("Harness Message", "Claude Code", AuthorKind::Named, true)
    } else {
        ("User Input", "Me", AuthorKind::Account, false)
    };
    out.push(NormalizedChatItem {
        author_kind,
        ..item(
            ids::record(source_id, uuid, ms.map(|m| m + n as i64)),
            author.to_string(),
            ms.map(|m| m + n as i64),
            text,
            label,
            aside,
        )
    });
}

/// The tags a record the harness writes in the person's place opens with,
/// in a transcript too old to say who wrote it in `origin`.
const HARNESS_TAGS: &[&str] = &[
    "<task-notification>",
    "<ci-monitor-event>",
    "<side-sessions-event>",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
];

/// Whether a `user` record is the harness's rather than the person's: its
/// own messages (`isMeta`), a summary it wrote when it compacted the
/// session, a subagent's brief (the parent agent's), anything whose
/// `origin` is not a person typing, and, before transcripts said so, a
/// notice by the tag it opens with.
fn written_by_the_harness(v: &Value, text: &str) -> bool {
    let flag = |key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
    let origin = v
        .get("origin")
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str);
    flag("isMeta")
        || flag("isCompactSummary")
        || flag("isSidechain")
        || match origin {
            Some(kind) => kind != "human",
            None => HARNESS_TAGS.iter().any(|t| text.starts_with(t)),
        }
}

/// The `<system-reminder>` spans the harness puts into a person's turn
/// (the instructions it loaded, a hook's notes, sessions it is watching),
/// and what is left, which is what the person typed.
fn split_reminders(text: &str) -> (Vec<String>, String) {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut reminders = Vec::new();
    let mut said = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        let Some(len) = rest[at..].find(CLOSE) else {
            break;
        };
        said.push_str(&rest[..at]);
        reminders.push(rest[at + OPEN.len()..at + len].trim().to_string());
        rest = &rest[at + len + CLOSE.len()..];
    }
    said.push_str(rest);
    reminders.retain(|r| !r.is_empty());
    (reminders, said.trim().to_string())
}

fn assistant_items(
    source_id: &str,
    uuid: &str,
    v: &Value,
    ms: Option<i64>,
    max_bytes: usize,
    out: &mut Vec<NormalizedChatItem>,
) {
    let named = v
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let model_handle = named.and_then(Handle::ai_model);
    let model = named.unwrap_or("Assistant").to_string();
    let mut text_parts: Vec<String> = Vec::new();
    for (i, b) in blocks_of(v).into_iter().enumerate() {
        let block_ms = ms.map(|m| m + i as i64);
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = str_of(b, "text") {
                    text_parts.push(t.trim_end().to_string());
                }
            }
            Some("thinking") => {
                let Some(thought) = str_of(b, "thinking").filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                let quoted = format!("> {}", thought.trim_end().replace('\n', "\n> "));
                out.push(NormalizedChatItem {
                    author_handle: model_handle.clone(),
                    ..item(
                        ids::thinking_block(source_id, uuid, i, block_ms),
                        model.clone(),
                        block_ms,
                        details("Thinking", &quoted),
                        "LLM Thinking",
                        true,
                    )
                });
            }
            Some("tool_use") => {
                let name = str_of(b, "name").unwrap_or("tool");
                let id = match str_of(b, "id") {
                    Some(tu) => ids::tool_use(source_id, uuid, tu, block_ms),
                    None => ids::block_fallback(source_id, uuid, i, block_ms),
                };
                let body = b
                    .get("input")
                    .map(|input| json_block(input, max_bytes))
                    .unwrap_or_default();
                out.push(item(
                    id,
                    model.clone(),
                    block_ms,
                    details(&format!("Tool use: {name}"), &body),
                    "Tool Call",
                    true,
                ));
            }
            _ => {}
        }
    }
    let text = text_parts.join("\n\n");
    if text.trim().is_empty() {
        return;
    }
    let ms = ms.map(|m| m + blocks_of(v).len() as i64);
    out.push(NormalizedChatItem {
        author_handle: model_handle,
        ..item(
            ids::record(source_id, uuid, ms),
            model,
            ms,
            text,
            "LLM Response",
            false,
        )
    });
}

/// Hook output and stop reasons. Only the ones that say something:
/// most `system` records are a stop-hook summary with nothing in it.
fn system_item(
    source_id: &str,
    uuid: &str,
    v: &Value,
    ms: Option<i64>,
) -> Option<NormalizedChatItem> {
    let subtype = str_of(v, "subtype").unwrap_or("system");
    let content = str_of(v, "content").filter(|s| !s.trim().is_empty());
    let errors: Vec<String> = v
        .get("hookErrors")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Value::to_string).collect())
        .unwrap_or_default();
    if content.is_none() && errors.is_empty() {
        return None;
    }
    let mut note = format!("system: {subtype}");
    if let Some(c) = content {
        note.push_str("\n\n");
        note.push_str(c.trim_end());
    }
    if !errors.is_empty() {
        note.push_str("\n\nhook errors: ");
        note.push_str(&errors.join(", "));
    }
    let mut it = item(
        ids::record(source_id, uuid, ms),
        "Claude Code".to_string(),
        ms,
        String::new(),
        "System",
        true,
    );
    it.kind = ItemKind::System;
    it.text = None;
    it.system_note = Some(note);
    Some(it)
}

/// The text of a tool result's `content`: a string, or the `text`
/// entries of a block list with a placeholder per non-text block.
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    Some("text") => parts.push(str_of(b, "text").unwrap_or("").to_string()),
                    Some(other) => parts.push(format!("[{other} content not shown]")),
                    None => parts.push(b.to_string()),
                }
            }
            parts.join("\n")
        }
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    }
}

fn blocks_of(record: &Value) -> Vec<&Value> {
    record
        .get("message")
        .and_then(|m| m.get("content"))
        .map(blocks_of_content)
        .unwrap_or_default()
}

fn blocks_of_content(content: &Value) -> Vec<&Value> {
    content
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_agent_sessions_render::titles;
    use serde_json::json;

    fn meta(tid: &str, title: &str, agent: Option<&str>) -> (String, Value) {
        let session = tid.split('#').next().unwrap();
        (
            tid.to_string(),
            json!({
                "session_id": session, "agent_id": agent, "title": title,
                "cwd": "/Users/picard/src/enterprise", "started_at": "2364-04-11T10:00:00.000Z",
                "cloud_session_id": null,
            }),
        )
    }

    fn rec(uuid: &str, ts: &str, body: Value) -> (String, Value) {
        let mut v = body;
        v["uuid"] = json!(uuid);
        v["timestamp"] = json!(ts);
        v["sessionId"] = json!("s1");
        (uuid.to_string(), v)
    }

    #[test]
    fn a_turn_reads_prompt_then_tool_traffic_then_answer() {
        let transcripts = vec![meta("s1", "Deflector realignment", None)];
        let records = vec![
            rec(
                "u1",
                "2364-04-11T10:00:00.000Z",
                json!({"type": "user", "message": {"content": "Realign the deflector dish"}}),
            ),
            rec(
                "a1",
                "2364-04-11T10:00:05.000Z",
                json!({"type": "assistant", "message": {"model": "claude-opus-5", "content": [
                    {"type": "thinking", "thinking": "Check the emitter first."},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "diag emitter"}}
                ]}}),
            ),
            rec(
                "u2",
                "2364-04-11T10:00:07.000Z",
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "emitter nominal"}
                ]}, "toolUseResult": {"stdout": "emitter nominal"}}),
            ),
            rec(
                "a2",
                "2364-04-11T10:00:09.000Z",
                json!({"type": "assistant", "message": {"model": "claude-opus-5", "content": [
                    {"type": "text", "text": "Dish realigned."}
                ]}}),
            ),
        ];
        let chats = build_chats("cc", &transcripts, &records, &titles(&transcripts), 1024);
        assert_eq!(chats.len(), 1);
        let c = &chats[0];
        assert_eq!(c.display, "Deflector realignment");
        assert_eq!(c.project.as_deref(), Some("enterprise"));
        assert_eq!(c.external_id.as_deref(), Some("s1"));
        let items = &c.buckets[0].items;
        let labels: Vec<&str> = items
            .iter()
            .map(|i| i.kind_label.as_deref().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "User Input",
                "LLM Thinking",
                "Tool Call",
                "Tool Result",
                "LLM Response"
            ]
        );
        let asides: Vec<bool> = items.iter().map(|i| i.is_aside).collect();
        assert_eq!(asides, [false, true, true, true, false]);
        let result = &items[3];
        assert!(result
            .text
            .as_deref()
            .unwrap()
            .contains("Tool result: Bash"));
        assert!(result.text.as_deref().unwrap().contains("emitter nominal"));
        assert_eq!(items[4].author_display, "claude-opus-5");
        // Inputs name the transcript row and every record row.
        assert!(c
            .inputs
            .iter()
            .any(|i| i.table == "transcripts" && i.id == "s1"));
        assert_eq!(c.inputs.iter().filter(|i| i.table == "records").count(), 4);
    }

    /// A tool's name comes from whoever wrote the tool; in a summary it
    /// is text, and its output stays inside its fence.
    #[test]
    fn a_tool_named_in_markup_renders_escaped() {
        let transcripts = vec![meta("s1", "Deflector realignment", None)];
        let records = vec![
            rec(
                "a1",
                "2364-04-11T10:00:05.000Z",
                json!({"type": "assistant", "message": {"model": "claude-opus-5", "content": [
                    {"type": "tool_use", "id": "t1", "name": "<script>x</script> & co", "input": {}}
                ]}}),
            ),
            rec(
                "u2",
                "2364-04-11T10:00:07.000Z",
                json!({"type": "user", "message": {"content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "```\n<script>x</script>"}
                ]}}),
            ),
        ];
        let chats = build_chats("cc", &transcripts, &records, &titles(&transcripts), 1024);
        let texts: Vec<&str> = chats[0].buckets[0]
            .items
            .iter()
            .filter_map(|i| i.text.as_deref())
            .collect();
        assert!(
            texts[0].starts_with(
                "<details><summary>Tool use: &lt;script&gt;x&lt;/script&gt; &amp; co</summary>"
            ),
            "{texts:?}"
        );
        assert!(
            texts[1].contains("<summary>Tool result: &lt;script&gt;x&lt;/script&gt; &amp; co"),
            "{texts:?}"
        );
        assert!(
            texts[1].contains("````\n```\n<script>x</script>\n````"),
            "{texts:?}"
        );
    }

    #[test]
    fn a_subagent_is_its_own_document_named_after_its_parent() {
        let transcripts = vec![
            meta("s1", "Deflector realignment", None),
            meta("s1#a9", "Scan the emitter logs", Some("a9")),
        ];
        let records = vec![
            rec(
                "u1",
                "2364-04-11T10:00:00.000Z",
                json!({"type": "user", "message": {"content": "Realign"}}),
            ),
            {
                let (id, mut v) = rec(
                    "u2",
                    "2364-04-11T10:00:01.000Z",
                    json!({"type": "user", "isSidechain": true, "message": {"content": "Scan the logs"}}),
                );
                v["agentId"] = json!("a9");
                (id, v)
            },
        ];
        let chats = build_chats("cc", &transcripts, &records, &titles(&transcripts), 1024);
        assert_eq!(chats.len(), 2);
        let agent = chats.iter().find(|c| c.id == "s1#a9").unwrap();
        assert_eq!(
            agent.display,
            "Scan the emitter logs — subagent of Deflector realignment"
        );
        assert_eq!(agent.buckets[0].items.len(), 1);
        assert_ne!(agent.chat_uuid, chats[0].chat_uuid);
        let parent = chats.iter().find(|c| c.id == "s1").unwrap();
        assert_eq!(
            parent.buckets[0].items.len(),
            1,
            "the subagent's records stay out of the parent"
        );
    }

    #[test]
    fn empty_stop_hook_summaries_and_bare_tool_only_turns_add_no_items() {
        let transcripts = vec![meta("s1", "t", None)];
        let records = vec![
            rec(
                "sys1",
                "2364-04-11T10:00:00.000Z",
                json!({"type": "system", "subtype": "stop_hook_summary", "hookErrors": []}),
            ),
            rec(
                "a1",
                "2364-04-11T10:00:01.000Z",
                json!({"type": "assistant", "message": {"model": "m", "content": [
                    {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "x"}}
                ]}}),
            ),
        ];
        let chats = build_chats("cc", &transcripts, &records, &titles(&transcripts), 1024);
        let labels: Vec<&str> = chats[0].buckets[0]
            .items
            .iter()
            .map(|i| i.kind_label.as_deref().unwrap())
            .collect();
        assert_eq!(
            labels,
            ["Tool Call"],
            "no empty LLM Response, no empty System"
        );
    }

    #[test]
    fn a_harness_message_is_an_aside_not_a_prompt() {
        let transcripts = vec![meta("s1", "t", None)];
        let records = vec![rec(
            "m1",
            "2364-04-11T10:00:00.000Z",
            json!({"type": "user", "isMeta": true, "message": {"content": "Caveat: …"}}),
        )];
        let it = &build_chats("cc", &transcripts, &records, &titles(&transcripts), 1024)[0].buckets
            [0]
        .items[0];
        assert_eq!(it.kind_label.as_deref(), Some("Harness Message"));
        assert!(it.is_aside);
    }

    /// The harness writes into the person's place too: notices it sends
    /// the agent, a subagent's brief, the reminders it prepends to a turn.
    /// None of it is "Me", or `from:me` finds the harness talking.
    #[test]
    fn what_the_harness_writes_is_not_the_persons_turn() {
        let transcripts = vec![meta("s1", "t", None)];
        let user = |uuid: &str, second: u32, extra: Value| {
            let mut v = json!({"type": "user"});
            for (k, x) in extra.as_object().unwrap() {
                v[k] = x.clone();
            }
            rec(uuid, &format!("2364-04-11T10:00:{second:02}.000Z"), v)
        };
        let records = vec![
            user(
                "typed",
                1,
                json!({"origin": {"kind": "human"}, "message": {"content": [
                    {"type": "text", "text": "<system-reminder>\nYou are in a worktree.\n</system-reminder>"},
                    {"type": "text", "text": "Raise the shields"}
                ]}}),
            ),
            user(
                "notice",
                2,
                json!({"origin": {"kind": "task-notification"},
                       "message": {"content": "<task-notification>scan done</task-notification>"}}),
            ),
            user(
                "event",
                3,
                json!({"message": {"content": "<ci-monitor-event>checks green</ci-monitor-event>"}}),
            ),
            user(
                "brief",
                4,
                json!({"isSidechain": true, "message": {"content": "Survey the nebula"}}),
            ),
            user("old", 5, json!({"message": {"content": "Engage"}})),
        ];
        let chats = build_chats("cc", &transcripts, &records, 1024);
        let said: Vec<(&str, &str, &str)> = chats[0].buckets[0]
            .items
            .iter()
            .map(|i| {
                (
                    i.kind_label.as_deref().unwrap(),
                    i.author_display.as_str(),
                    i.text.as_deref().unwrap_or(""),
                )
            })
            .collect();
        assert_eq!(
            said,
            [
                ("Harness Message", "Claude Code", "You are in a worktree."),
                ("User Input", "Me", "Raise the shields"),
                (
                    "Harness Message",
                    "Claude Code",
                    "<task-notification>scan done</task-notification>"
                ),
                (
                    "Harness Message",
                    "Claude Code",
                    "<ci-monitor-event>checks green</ci-monitor-event>"
                ),
                ("Harness Message", "Claude Code", "Survey the nebula"),
                ("User Input", "Me", "Engage"),
            ]
        );
    }

    #[test]
    fn reminders_split_from_what_was_typed_wherever_they_sit() {
        let (reminders, said) = split_reminders(
            "<system-reminder>a</system-reminder>Make it so<system-reminder> b </system-reminder>",
        );
        assert_eq!(reminders, ["a", "b"]);
        assert_eq!(said, "Make it so");
        let (reminders, said) = split_reminders("Engage <system-reminder>unclosed");
        assert!(reminders.is_empty());
        assert_eq!(said, "Engage <system-reminder>unclosed");
    }

    /// A deleted transcript has no row to mint its document id from, so
    /// the id has to come from the raw id alone — and agree with the one
    /// the chat was rendered under.
    #[test]
    fn a_transcripts_document_id_needs_no_row() {
        let transcripts = vec![meta("s1", "t", None), meta("s1#a9", "u", Some("a9"))];
        let chats = build_chats("cc", &transcripts, &[], &titles(&transcripts), 1024);
        for c in &chats {
            assert_eq!(c.chat_uuid, chat_uuid_of("cc", &c.id), "{}", c.id);
        }
    }
}
