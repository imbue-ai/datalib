//! One document per transcript, through the shared chat renderer. A
//! session reads as what was said, with each run of tool calls and
//! results folded into one collapsed block; a subagent's transcript is
//! its own document.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, Bucket, Buckets, RenderProfile,
};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Inputs, RawRange};
use serde_json::Value;

use datalib_etl_claude_code::ingest::parse::{human_text, transcript_id};
use datalib_etl_claude_code::ingest::{db_path_for, RawDb};
use datalib_schema::providers::Provider;

use crate::ids;

/// v2: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout).
pub const RENDER_VERSION: u32 = 2;

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
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    range: RawRange<'_>,
    max_tool_result_bytes: usize,
) -> Result<RenderOutcome> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderOutcome::default());
    }
    let (transcripts, records, scan) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Pinned at open; no commit means nothing committed to render.
            let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
                return Ok(Default::default());
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            let loaded = async {
                let transcripts = datalib_etl::doltlite_raw::load_payloads_with_id(
                    db.pool(),
                    datalib_etl::pin::Reads::At(&pin),
                    "transcripts",
                )
                .await?;
                let records = datalib_etl::doltlite_raw::load_payloads_with_id(
                    db.pool(),
                    datalib_etl::pin::Reads::At(&pin),
                    "records",
                )
                .await?;
                let scan = scan_diff(db.pool(), range.cursor, &pin).await?;
                anyhow::Ok((transcripts, records, scan))
            }
            .await;
            db.close().await;
            loaded
        })
    })?;

    // No early return on an empty store: a run that deleted every
    // transcript still has to declare their buckets empty so the
    // documents go.
    let all_chats = build_chats(source_id, &transcripts, &records, max_tool_result_bytes);

    let mut outcome = RenderOutcome {
        new_head: scan.new_head.clone(),
        scan_elapsed: scan.scan_elapsed,
        ..Default::default()
    };
    let by_uuid: HashMap<&str, &str> = all_chats
        .iter()
        .map(|c| (c.chat_uuid.as_str(), c.id.as_str()))
        .collect();
    let narrowed = range.narrow_by(scan.render.as_ref(), |key| {
        by_uuid.get(key).map(|id| id.to_string())
    });
    // The bucket key is minted from the raw id alone, so a transcript
    // the diff names as deleted is still declared — with no documents,
    // which is what removes the ones it had.
    outcome.buckets = narrowed
        .render
        .iter()
        .flatten()
        .map(|tid| chat_uuid_of(source_id, tid))
        .chain(narrowed.gone.iter().cloned())
        .map(|key| Bucket {
            key,
            inputs: Vec::new(),
        })
        .collect();
    let chats: Vec<NormalizedChat> = match &narrowed.render {
        None => all_chats,
        Some(changed) => {
            let before = all_chats.len();
            let kept: Vec<NormalizedChat> = all_chats
                .into_iter()
                .filter(|c| changed.contains(&c.id))
                .collect();
            outcome.skipped = before.saturating_sub(kept.len());
            kept
        }
    };
    let s = cc_render_all(
        &profile(),
        &chats,
        out_root,
        source_id,
        &HashMap::new(),
        progress,
        on_doc_complete,
    )?;
    outcome.rendered = s.docs_rendered;
    outcome.buckets.extend(s.buckets);
    Ok(outcome)
}

#[derive(Debug, Clone, Default)]
pub struct RenderOutcome {
    pub rendered: usize,
    pub skipped: usize,
    pub new_head: Option<String>,
    pub scan_elapsed: Option<std::time::Duration>,
    pub buckets: Buckets,
}

/// Which transcripts moved since `last_render_hash`. The bucket is the
/// raw store's transcript id, which is also `NormalizedChat::id`.
async fn scan_diff(
    pool: &sqlx::SqlitePool,
    last_render_hash: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<datalib_etl::doltlite_raw::DiffScan> {
    datalib_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT bucket FROM (
                    SELECT coalesce(to_transcript_id, from_transcript_id) AS bucket
                      FROM dolt_diff_records
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_id, from_id)
                      FROM dolt_diff_transcripts
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE bucket IS NOT NULL
            ",
        },
    )
    .await
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
    max_tool_result_bytes: usize,
) -> Vec<NormalizedChat> {
    let mut by_transcript: BTreeMap<String, Vec<(&str, &Value)>> = BTreeMap::new();
    for (row_id, v) in records {
        let session = str_of(v, "sessionId").unwrap_or("");
        let key = transcript_id(session, str_of(v, "agentId"));
        by_transcript.entry(key).or_default().push((row_id, v));
    }
    let titles: HashMap<&str, &str> = transcripts
        .iter()
        .filter_map(|(id, meta)| str_of(meta, "title").map(|t| (id.as_str(), t)))
        .collect();

    let mut chats = Vec::with_capacity(transcripts.len());
    for (tid, meta) in transcripts {
        let mut rows = by_transcript.remove(tid).unwrap_or_default();
        rows.sort_by(|a, b| {
            (str_of(a.1, "timestamp").unwrap_or(""), a.0)
                .cmp(&(str_of(b.1, "timestamp").unwrap_or(""), b.0))
        });
        let parent_title = str_of(meta, "agent_id")
            .and_then(|_| str_of(meta, "session_id"))
            .and_then(|s| titles.get(s).copied());
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
        path_prefix: None,
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
    let is_meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);
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
            "tool_result",
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
    let mut text = human_text(content).unwrap_or_default();
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
    let (label, author, aside) = if is_meta {
        ("Harness Message", "Claude Code", true)
    } else {
        ("User Input", "User", false)
    };
    out.push(item(
        ids::record(source_id, uuid, ms.map(|m| m + n as i64)),
        "user",
        author.to_string(),
        ms.map(|m| m + n as i64),
        text,
        label,
        aside,
    ));
}

fn assistant_items(
    source_id: &str,
    uuid: &str,
    v: &Value,
    ms: Option<i64>,
    max_bytes: usize,
    out: &mut Vec<NormalizedChatItem>,
) {
    let model = v
        .get("message")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("Assistant")
        .to_string();
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
                out.push(item(
                    ids::thinking_block(source_id, uuid, i, block_ms),
                    "thinking",
                    model.clone(),
                    block_ms,
                    details("Thinking", &quoted),
                    "LLM Thinking",
                    true,
                ));
            }
            Some("tool_use") => {
                let name = str_of(b, "name").unwrap_or("tool");
                let id = match str_of(b, "id") {
                    Some(tu) => ids::tool_use(source_id, uuid, tu, block_ms),
                    None => ids::block_fallback(source_id, uuid, i, block_ms),
                };
                let body = match b.get("input") {
                    Some(input) if !json_is_empty(input) => {
                        let pretty =
                            serde_json::to_string_pretty(&canonicalize(input)).unwrap_or_default();
                        format!("```json\n{}\n```", clamp(&pretty, max_bytes))
                    }
                    _ => String::new(),
                };
                out.push(item(
                    id,
                    "tool_use",
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
    out.push(item(
        ids::record(source_id, uuid, ms),
        "assistant",
        model,
        ms,
        text,
        "LLM Response",
        false,
    ));
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
        "system",
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

fn item(
    id: datalib_id::Identity,
    author_id: &str,
    author_display: String,
    date_ms: Option<i64>,
    text: String,
    kind_label: &str,
    is_aside: bool,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_id: author_id.to_string(),
        author_display,
        date_ms,
        text: (!text.trim().is_empty()).then_some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside,
        unread: false,
        problems: Vec::new(),
    }
}

fn details(summary: &str, body: &str) -> String {
    if body.trim().is_empty() {
        format!("<details><summary>{summary}</summary>\n\n</details>")
    } else {
        format!("<details><summary>{summary}</summary>\n\n{body}\n\n</details>")
    }
}

fn fenced(s: &str) -> String {
    if s.trim().is_empty() {
        return String::new();
    }
    // A fence longer than any run of backticks in the body, so a tool
    // result that itself contains ``` cannot close it early.
    let longest = s.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}", s.trim_end())
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

/// Cut on a char boundary and say what was cut.
pub fn clamp(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n… [{} more bytes not shown; raise max_tool_result_bytes and re-render to see them]",
        &s[..end],
        s.len() - end
    )
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

/// The last path component of the working directory: what a person
/// would call the project.
fn project_of(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn iso_to_ms(s: &str) -> Option<i64> {
    datalib_time::parse_strict(s)
        .ok()
        .map(|t| t.to_unix_millis())
}

fn json_is_empty(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Null => true,
        _ => false,
    }
}

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(pairs.len());
            for (k, val) in pairs {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let chats = build_chats("cc", &transcripts, &records, 1024);
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
        let chats = build_chats("cc", &transcripts, &records, 1024);
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
        let chats = build_chats("cc", &transcripts, &records, 1024);
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
        let it = &build_chats("cc", &transcripts, &records, 1024)[0].buckets[0].items[0];
        assert_eq!(it.kind_label.as_deref(), Some("Harness Message"));
        assert!(it.is_aside);
    }

    #[test]
    fn long_tool_results_are_cut_and_say_so() {
        let s = "x".repeat(100);
        let out = clamp(&s, 10);
        assert!(out.starts_with("xxxxxxxxxx\n"));
        assert!(out.contains("90 more bytes"));
        assert_eq!(clamp("short", 10), "short");
        // Cut lands on a char boundary.
        let e = "é".repeat(10);
        assert!(clamp(&e, 3).starts_with("é\n"));
    }

    #[test]
    fn a_fence_outlasts_backticks_in_the_body() {
        let f = fenced("a\n```\nb");
        assert!(f.starts_with("````\n"), "{f}");
        assert!(f.ends_with("\n````"), "{f}");
    }

    /// A deleted transcript has no row to mint its document id from, so
    /// the id has to come from the raw id alone — and agree with the one
    /// the chat was rendered under.
    #[test]
    fn a_transcripts_document_id_needs_no_row() {
        let transcripts = vec![meta("s1", "t", None), meta("s1#a9", "u", Some("a9"))];
        let chats = build_chats("cc", &transcripts, &[], 1024);
        for c in &chats {
            assert_eq!(c.chat_uuid, chat_uuid_of("cc", &c.id), "{}", c.id);
        }
    }

    #[test]
    fn project_is_the_last_path_component() {
        assert_eq!(project_of("/Users/picard/src/enterprise"), "enterprise");
        assert_eq!(project_of("/Users/picard/src/enterprise/"), "enterprise");
        assert_eq!(project_of("/"), "/");
    }
}
