//! One document per thread, through the shared chat renderer. A thread
//! reads as what was said, with each run of tool calls and outputs
//! folded into one collapsed block; a sub-agent's thread is its own
//! document. Only the `response_item` lines carry content the page
//! shows — the `event_msg` lines repeat them for the UI — but the
//! `user_message` events are the one record of what the person
//! actually typed, as against what Codex injected under the user role.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use datalib_id::Identity;
use serde_json::Value;

use datalib_etl_codex::ingest::parse::is_typed_message;
use datalib_etl_codex::ingest::{db_path_for, RawDb};
use datalib_schema::providers::Provider;

use crate::ids;

pub const RENDER_VERSION: u32 = 1;

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Codex,
        source_label: "Codex".to_string(),
        chat_kind: "Codex Thread".to_string(),
        // Per-item kind is always set via `kind_label`; nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Codex Reaction".to_string(),
        chat_entity_kind: ids::KIND_THREAD,
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
                let records = load_records(db.pool(), &pin).await?;
                let scan = scan_diff(db.pool(), range.cursor, &pin).await?;
                anyhow::Ok((transcripts, records, scan))
            }
            .await;
            db.close().await;
            loaded
        })
    })?;

    // No early return on an empty store: a run that deleted every
    // thread still has to declare their buckets empty so the documents
    // go.
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
    // The bucket key is minted from the raw id alone, so a thread the
    // diff names as deleted is still declared — with no documents,
    // which is what removes the ones it had.
    outcome.buckets = narrowed
        .render
        .iter()
        .flatten()
        .map(|tid| ids::thread(source_id, tid).uuid)
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

/// One raw line: its row id, the thread, its number, the line.
pub type RecordRow = (String, String, i64, Value);

async fn load_records(
    pool: &sqlx::SqlitePool,
    pin: &datalib_etl::pin::Pin,
) -> Result<Vec<RecordRow>> {
    let view = datalib_etl::pin::Reads::At(pin).table("records");
    // Safe: `view` is a pinned-table name the pin module composes from
    // a `&'static str`; nothing here comes from data.
    let sql = format!(
        "SELECT id, transcript_id, line_no, json(payload) FROM {view} ORDER BY transcript_id, line_no"
    );
    let rows: Vec<(String, String, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await?;
    rows.into_iter()
        .map(|(id, tid, n, p)| Ok((id, tid, n, serde_json::from_str(&p)?)))
        .collect()
}

/// Which threads moved since `last_render_hash`. The bucket is the raw
/// store's transcript id, which is also `NormalizedChat::id`.
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

fn build_chats(
    source_id: &str,
    transcripts: &[(String, Value)],
    records: &[RecordRow],
    max_tool_result_bytes: usize,
) -> Vec<NormalizedChat> {
    let mut by_thread: BTreeMap<&str, Vec<&RecordRow>> = BTreeMap::new();
    for r in records {
        by_thread.entry(r.1.as_str()).or_default().push(r);
    }
    let titles: HashMap<&str, &str> = transcripts
        .iter()
        .filter_map(|(id, meta)| str_of(meta, "title").map(|t| (id.as_str(), t)))
        .collect();

    let mut chats = Vec::with_capacity(transcripts.len());
    for (tid, meta) in transcripts {
        let mut rows = by_thread.remove(tid.as_str()).unwrap_or_default();
        rows.sort_by_key(|r| r.2);
        let parent_title = str_of(meta, "parent_thread_id").and_then(|p| titles.get(p).copied());
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
    rows: &[&RecordRow],
    parent_title: Option<&str>,
    max_tool_result_bytes: usize,
) -> NormalizedChat {
    let inputs = Inputs::default();
    inputs.read("transcripts", tid);

    // A tool output names only the call it answers; the tool's name is
    // on the call. And what the person typed is only told apart from
    // what Codex injected under the user role by the UI's own events.
    let mut tool_names: HashMap<&str, &str> = HashMap::new();
    let mut typed: HashSet<&str> = HashSet::new();
    for r in rows {
        let p = &r.3["payload"];
        match str_of(&r.3, "type") {
            Some("response_item") => {
                let name = match str_of(p, "type") {
                    Some("function_call") | Some("custom_tool_call") => str_of(p, "name"),
                    Some("local_shell_call") => Some("shell"),
                    _ => None,
                };
                if let (Some(id), Some(name)) = (str_of(p, "call_id"), name) {
                    tool_names.insert(id, name);
                }
            }
            Some("event_msg") if str_of(p, "type") == Some("user_message") => {
                if let Some(m) = str_of(p, "message") {
                    typed.insert(m.trim());
                }
            }
            _ => {}
        }
    }

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    let mut model: Option<String> = None;
    let mut last_ms: Option<i64> = None;
    for r in rows {
        let (row_id, _, line_no, v) = r;
        inputs.read("records", row_id);
        // Never earlier than the line before it: a rollout is written
        // in order, and several lines can share one millisecond.
        let ms = match (str_of(v, "timestamp").and_then(iso_to_ms), last_ms) {
            (Some(t), Some(p)) => Some(t.max(p + 1)),
            (Some(t), None) => Some(t),
            (None, Some(p)) => Some(p + 1),
            (None, None) => None,
        };
        last_ms = ms.or(last_ms);
        let p = &v["payload"];
        match str_of(v, "type") {
            Some("turn_context") => {
                if let Some(m) = str_of(p, "model") {
                    model = Some(m.to_string());
                }
            }
            Some("response_item") => {
                if let Some(it) = response_item(
                    source_id,
                    tid,
                    *line_no,
                    p,
                    ms,
                    model.as_deref(),
                    &tool_names,
                    &typed,
                    max_tool_result_bytes,
                ) {
                    items.push(it);
                }
            }
            Some("compacted") => {
                if let Some(it) =
                    compacted_item(source_id, tid, *line_no, p, ms, max_tool_result_bytes)
                {
                    items.push(it);
                }
            }
            _ => {}
        }
    }

    let own_title = str_of(meta, "title").unwrap_or("(untitled)").to_string();
    let is_subagent = str_of(meta, "parent_thread_id").is_some();
    let display = match (is_subagent, parent_title) {
        (true, Some(parent)) => format!("{own_title} — sub-agent of {parent}"),
        (true, None) => format!("{own_title} — sub-agent"),
        (false, _) => own_title.clone(),
    };
    let id = ids::thread(source_id, tid);
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
        source_url: None,
        upstream_account: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: id.uuid,
            // One document per thread, keyed on the thread's own id and
            // kind, so the row's backpointer is the chat's.
            source_ref: None,
            items,
        }],
        inputs: inputs.declared(),
    }
}

#[allow(clippy::too_many_arguments)]
fn response_item(
    source_id: &str,
    tid: &str,
    line_no: i64,
    p: &Value,
    ms: Option<i64>,
    model: Option<&str>,
    tool_names: &HashMap<&str, &str>,
    typed: &HashSet<&str>,
    max_bytes: usize,
) -> Option<NormalizedChatItem> {
    let model = model.unwrap_or("Codex");
    match str_of(p, "type")? {
        "message" => {
            let (mut text, images) = message_text(p.get("content"));
            if images > 0 {
                text = format!("{text}\n\n*[{images} image(s) not shown]*")
                    .trim()
                    .to_string();
            }
            if text.trim().is_empty() {
                return None;
            }
            // What the person typed, as against what Codex injected
            // under the same role: the record's own tags where it has
            // them, else the UI events an older Codex wrote, else the
            // shape of the text.
            let typed_here = || {
                is_typed_message(p)
                    .unwrap_or_else(|| typed.contains(text.trim()) || !looks_injected(&text))
            };
            let (author_id, author, label, aside) = match str_of(p, "role") {
                Some("assistant") => ("assistant", model, "LLM Response", false),
                Some("user") if typed_here() => ("user", "User", "User Input", false),
                _ => ("system", "Codex", "Harness Message", true),
            };
            // An injected blob — a permissions primer, a whole AGENTS.md —
            // is cut like a tool output: the store keeps it, the page
            // keeps this much.
            if aside {
                text = clamp(&text, max_bytes);
            }
            Some(item(
                ids::record(source_id, tid, line_no, ms),
                author_id,
                author.to_string(),
                ms,
                text,
                label,
                aside,
            ))
        }
        "reasoning" => {
            let thought = reasoning_text(p);
            if thought.trim().is_empty() {
                return None;
            }
            let quoted = format!("> {}", thought.trim_end().replace('\n', "\n> "));
            Some(item(
                ids::record(source_id, tid, line_no, ms),
                "thinking",
                model.to_string(),
                ms,
                details("Thinking", &quoted),
                "LLM Thinking",
                true,
            ))
        }
        "function_call" => {
            let name = str_of(p, "name").unwrap_or("tool");
            let body = str_of(p, "arguments")
                .map(|a| fenced_json_or_text(a, max_bytes))
                .unwrap_or_default();
            Some(tool_call(source_id, tid, line_no, p, ms, model, name, body))
        }
        "local_shell_call" => {
            let command = p
                .get("action")
                .and_then(|a| a.get("command"))
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            Some(tool_call(
                source_id,
                tid,
                line_no,
                p,
                ms,
                model,
                "shell",
                fenced(&clamp(&command, max_bytes)),
            ))
        }
        "custom_tool_call" => {
            let name = str_of(p, "name").unwrap_or("tool");
            let body = str_of(p, "input")
                .map(|i| fenced(&clamp(i, max_bytes)))
                .unwrap_or_default();
            Some(tool_call(source_id, tid, line_no, p, ms, model, name, body))
        }
        "web_search_call" => {
            let body = p
                .get("action")
                .filter(|a| !a.is_null())
                .map(|a| fenced_json_or_text(&a.to_string(), max_bytes))
                .unwrap_or_default();
            Some(tool_call(
                source_id,
                tid,
                line_no,
                p,
                ms,
                model,
                "web_search",
                body,
            ))
        }
        "function_call_output" | "custom_tool_call_output" => {
            let call_id = str_of(p, "call_id").unwrap_or("");
            let name = tool_names
                .get(call_id)
                .copied()
                .or_else(|| str_of(p, "name"))
                .unwrap_or("tool");
            let (text, failed) = output_text(p.get("output"));
            let summary = if failed {
                format!("Tool result: {name} (error)")
            } else {
                format!("Tool result: {name}")
            };
            let id = if call_id.is_empty() {
                ids::record(source_id, tid, line_no, ms)
            } else {
                ids::tool_result(source_id, tid, call_id, ms)
            };
            Some(item(
                id,
                "tool_result",
                name.to_string(),
                ms,
                details(&summary, &fenced(&clamp(&text, max_bytes))),
                "Tool Result",
                true,
            ))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn tool_call(
    source_id: &str,
    tid: &str,
    line_no: i64,
    p: &Value,
    ms: Option<i64>,
    model: &str,
    name: &str,
    body: String,
) -> NormalizedChatItem {
    let id = match str_of(p, "call_id") {
        Some(c) => ids::tool_use(source_id, tid, c, ms),
        None => ids::record(source_id, tid, line_no, ms),
    };
    item(
        id,
        "tool_use",
        model.to_string(),
        ms,
        details(&format!("Tool call: {name}"), &body),
        "Tool Call",
        true,
    )
}

/// Codex folded the history into a summary; the summary is what the
/// model saw from then on.
fn compacted_item(
    source_id: &str,
    tid: &str,
    line_no: i64,
    p: &Value,
    ms: Option<i64>,
    max_bytes: usize,
) -> Option<NormalizedChatItem> {
    let message = str_of(p, "message").filter(|m| !m.trim().is_empty())?;
    let mut it = item(
        ids::record(source_id, tid, line_no, ms),
        "system",
        "Codex".to_string(),
        ms,
        String::new(),
        "System",
        true,
    );
    it.kind = ItemKind::System;
    it.text = None;
    it.system_note = Some(format!(
        "context compacted\n\n{}",
        clamp(message.trim_end(), max_bytes)
    ));
    Some(it)
}

/// The text of a message's content items and how many images rode
/// along.
fn message_text(content: Option<&Value>) -> (String, usize) {
    let mut parts = Vec::new();
    let mut images = 0;
    for b in content.and_then(Value::as_array).into_iter().flatten() {
        match str_of(b, "type") {
            Some("input_text") | Some("output_text") => {
                if let Some(t) = str_of(b, "text") {
                    parts.push(t.trim_end().to_string());
                }
            }
            Some("input_image") => images += 1,
            _ => {}
        }
    }
    (parts.join("\n\n"), images)
}

/// What Codex puts under the user role that nobody typed: its
/// instructions files and environment notes, each wrapped in a tag or
/// headed by a name.
fn looks_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<') || t.starts_with("# AGENTS.md instructions")
}

fn reasoning_text(p: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["summary", "content"] {
        for b in p.get(key).and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = str_of(b, "text").filter(|t| !t.trim().is_empty()) {
                parts.push(t.trim_end().to_string());
            }
        }
    }
    parts.join("\n\n")
}

/// A tool's output and whether it reported failure. On the wire it is
/// a string or a list of content items; an older Codex wrapped a
/// shell's output in a JSON string with its exit code beside it.
fn output_text(output: Option<&Value>) -> (String, bool) {
    match output {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(m)) if m.get("output").is_some_and(Value::is_string) => {
                let exit_code = m
                    .get("metadata")
                    .and_then(|md| md.get("exit_code"))
                    .and_then(Value::as_i64);
                (
                    m["output"].as_str().unwrap_or("").to_string(),
                    exit_code.is_some_and(|c| c != 0),
                )
            }
            _ => (s.clone(), false),
        },
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for b in blocks {
                match str_of(b, "type") {
                    Some("input_text") | Some("text") | Some("output_text") => {
                        parts.push(str_of(b, "text").unwrap_or("").to_string())
                    }
                    Some(other) => parts.push(format!("[{other} content not shown]")),
                    None => parts.push(b.to_string()),
                }
            }
            (parts.join("\n"), false)
        }
        Some(v) if !v.is_null() => (v.to_string(), false),
        _ => (String::new(), false),
    }
}

fn item(
    id: Identity,
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
    // output that itself contains ``` cannot close it early.
    let longest = s.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}\n{}\n{fence}", s.trim_end())
}

/// A tool's arguments arrive as a string holding JSON; show it
/// pretty-printed with sorted keys when it is, verbatim when it is not.
fn fenced_json_or_text(s: &str, max_bytes: usize) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(v) if !json_is_empty(&v) => {
            let pretty = serde_json::to_string_pretty(&canonicalize(&v)).unwrap_or_default();
            format!("```json\n{}\n```", clamp(&pretty, max_bytes))
        }
        Ok(_) => String::new(),
        Err(_) => fenced(&clamp(s, max_bytes)),
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

    fn meta(tid: &str, title: &str, parent: Option<&str>) -> (String, Value) {
        (
            tid.to_string(),
            json!({
                "thread_id": tid, "parent_thread_id": parent, "title": title,
                "cwd": "/Users/picard/src/enterprise", "started_at": "2364-04-11T10:00:00.000Z",
            }),
        )
    }

    fn rec(tid: &str, n: i64, ts: &str, kind: &str, payload: Value) -> RecordRow {
        (
            format!("{tid}#{n}"),
            tid.to_string(),
            n,
            json!({"timestamp": ts, "type": kind, "payload": payload}),
        )
    }

    fn user(text: &str) -> Value {
        json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]})
    }

    #[test]
    fn a_turn_reads_prompt_then_tool_traffic_then_answer() {
        let transcripts = vec![meta("t1", "Realign the deflector dish", None)];
        let t = "2364-04-11T10:00:00.000Z";
        let records = vec![
            rec("t1", 1, t, "session_meta", json!({"id": "t1"})),
            rec("t1", 2, t, "response_item", json!({"type": "message", "role": "developer", "content": [{"type": "input_text", "text": "<permissions instructions>\nsandboxed"}]})),
            rec("t1", 3, t, "response_item", user("# AGENTS.md instructions for /Users/picard/src/enterprise\n\nRun the diagnostics first.")),
            rec("t1", 4, t, "turn_context", json!({"turn_id": "u1", "model": "gpt-5.3-codex"})),
            rec("t1", 5, t, "response_item", user("Realign the deflector dish")),
            rec("t1", 6, t, "event_msg", json!({"type": "user_message", "message": "Realign the deflector dish"})),
            rec("t1", 7, "2364-04-11T10:00:02.000Z", "response_item", json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "**Checking the emitter first**"}], "encrypted_content": "xxx"})),
            rec("t1", 8, "2364-04-11T10:00:03.000Z", "response_item", json!({"type": "function_call", "name": "shell", "arguments": "{\"command\":[\"bash\",\"-lc\",\"diag emitter\"]}", "call_id": "call_1"})),
            rec("t1", 9, "2364-04-11T10:00:04.000Z", "response_item", json!({"type": "function_call_output", "call_id": "call_1", "output": "{\"output\":\"emitter nominal\",\"metadata\":{\"exit_code\":0,\"duration_seconds\":0.2}}"})),
            rec("t1", 10, "2364-04-11T10:00:05.000Z", "response_item", json!({"type": "custom_tool_call", "name": "apply_patch", "call_id": "call_2", "input": "*** Begin Patch\n*** Update File: dish.conf\n@@\n-threshold=0.5\n+threshold=0.3\n*** End Patch"})),
            rec("t1", 11, "2364-04-11T10:00:06.000Z", "response_item", json!({"type": "custom_tool_call_output", "call_id": "call_2", "output": "Success. Updated the following files:\nM dish.conf"})),
            rec("t1", 12, "2364-04-11T10:00:07.000Z", "response_item", json!({"type": "message", "role": "assistant", "phase": "final_answer", "content": [{"type": "output_text", "text": "Dish realigned."}]})),
            rec("t1", 13, "2364-04-11T10:00:07.000Z", "event_msg", json!({"type": "agent_message", "message": "Dish realigned."})),
            rec("t1", 14, "2364-04-11T10:00:07.000Z", "event_msg", json!({"type": "task_complete", "turn_id": "u1"})),
        ];
        let chats = build_chats("codex", &transcripts, &records, 1024);
        assert_eq!(chats.len(), 1);
        let c = &chats[0];
        assert_eq!(c.display, "Realign the deflector dish");
        assert_eq!(c.project.as_deref(), Some("enterprise"));
        assert_eq!(c.external_id.as_deref(), Some("t1"));
        let items = &c.buckets[0].items;
        let labels: Vec<&str> = items
            .iter()
            .map(|i| i.kind_label.as_deref().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Harness Message",
                "Harness Message",
                "User Input",
                "LLM Thinking",
                "Tool Call",
                "Tool Result",
                "Tool Call",
                "Tool Result",
                "LLM Response"
            ]
        );
        let asides: Vec<bool> = items.iter().map(|i| i.is_aside).collect();
        assert_eq!(
            asides,
            [true, true, false, true, true, true, true, true, false]
        );
        assert!(items[4]
            .text
            .as_deref()
            .unwrap()
            .contains("Tool call: shell"));
        assert!(items[4].text.as_deref().unwrap().contains("diag emitter"));
        let result = items[5].text.as_deref().unwrap();
        assert!(result.contains("Tool result: shell"), "{result}");
        assert!(result.contains("emitter nominal"));
        assert!(
            !result.contains("exit_code"),
            "the wrapped output is unwrapped: {result}"
        );
        assert!(items[7]
            .text
            .as_deref()
            .unwrap()
            .contains("Tool result: apply_patch"));
        assert_eq!(items[8].author_display, "gpt-5.3-codex");
        // The event lines duplicate nothing on the page.
        assert!(!items.iter().any(|i| i.author_id == "event"));
        // Stamps never run backwards, though most lines share a second.
        let stamps: Vec<i64> = items.iter().map(|i| i.date_ms.unwrap()).collect();
        assert!(stamps.windows(2).all(|w| w[0] < w[1]), "{stamps:?}");
        // Inputs name the transcript row and every record row.
        assert!(c
            .inputs
            .iter()
            .any(|i| i.table == "transcripts" && i.id == "t1"));
        assert_eq!(c.inputs.iter().filter(|i| i.table == "records").count(), 14);
    }

    /// A prompt the person typed is a prompt even when it starts with
    /// a tag, because the UI's event says they typed it.
    #[test]
    fn what_the_person_typed_is_never_a_harness_message() {
        let transcripts = vec![meta("t1", "t", None)];
        let t = "2364-04-11T10:00:00.000Z";
        let records = vec![
            rec("t1", 1, t, "response_item", user("<html> is what I want")),
            rec(
                "t1",
                2,
                t,
                "event_msg",
                json!({"type": "user_message", "message": "<html> is what I want"}),
            ),
            rec(
                "t1",
                3,
                t,
                "response_item",
                user("<environment_context>\n<cwd>/x</cwd>\n</environment_context>"),
            ),
        ];
        let items = build_chats("codex", &transcripts, &records, 1024)
            .remove(0)
            .buckets
            .remove(0)
            .items;
        assert_eq!(items[0].kind_label.as_deref(), Some("User Input"));
        assert_eq!(items[1].kind_label.as_deref(), Some("Harness Message"));
    }

    #[test]
    fn a_subagent_is_its_own_document_named_after_its_parent() {
        let transcripts = vec![
            meta("t1", "Realign the deflector dish", None),
            meta("t2", "Data", Some("t1")),
        ];
        let t = "2364-04-11T10:00:00.000Z";
        let records = vec![
            rec("t1", 1, t, "response_item", user("Realign")),
            rec("t2", 1, t, "response_item", user("Scan the logs")),
        ];
        let chats = build_chats("codex", &transcripts, &records, 1024);
        assert_eq!(chats.len(), 2);
        let agent = chats.iter().find(|c| c.id == "t2").unwrap();
        assert_eq!(
            agent.display,
            "Data — sub-agent of Realign the deflector dish"
        );
        assert_eq!(agent.buckets[0].items.len(), 1);
        assert_ne!(agent.chat_uuid, chats[0].chat_uuid);
        assert_eq!(chats[0].buckets[0].items.len(), 1);
    }

    #[test]
    fn a_failed_shell_call_and_a_compaction_say_so() {
        let transcripts = vec![meta("t1", "t", None)];
        let t = "2364-04-11T10:00:00.000Z";
        let records = vec![
            rec(
                "t1",
                1,
                t,
                "response_item",
                json!({"type": "local_shell_call", "call_id": "c1", "status": "completed", "action": {"type": "exec", "command": ["bash", "-lc", "false"]}}),
            ),
            rec(
                "t1",
                2,
                t,
                "response_item",
                json!({"type": "function_call_output", "call_id": "c1", "output": "{\"output\":\"\",\"metadata\":{\"exit_code\":1,\"duration_seconds\":0.1}}"}),
            ),
            rec(
                "t1",
                3,
                t,
                "compacted",
                json!({"message": "Earlier: the dish was realigned."}),
            ),
            rec(
                "t1",
                4,
                t,
                "response_item",
                json!({"type": "reasoning", "summary": [], "encrypted_content": "xxx"}),
            ),
        ];
        let items = build_chats("codex", &transcripts, &records, 1024)
            .remove(0)
            .buckets
            .remove(0)
            .items;
        assert_eq!(items.len(), 3, "redacted reasoning adds no item");
        assert!(items[0]
            .text
            .as_deref()
            .unwrap()
            .contains("Tool call: shell"));
        assert!(items[0].text.as_deref().unwrap().contains("bash -lc false"));
        assert!(items[1]
            .text
            .as_deref()
            .unwrap()
            .contains("Tool result: shell (error)"));
        assert_eq!(items[2].kind, ItemKind::System);
        assert!(items[2]
            .system_note
            .as_deref()
            .unwrap()
            .contains("context compacted"));
    }

    #[test]
    fn long_tool_results_are_cut_and_say_so() {
        let s = "x".repeat(100);
        let out = clamp(&s, 10);
        assert!(out.starts_with("xxxxxxxxxx\n"));
        assert!(out.contains("90 more bytes"));
        assert_eq!(clamp("short", 10), "short");
        let e = "é".repeat(10);
        assert!(clamp(&e, 3).starts_with("é\n"));
    }

    #[test]
    fn a_fence_outlasts_backticks_in_the_body() {
        let f = fenced("a\n```\nb");
        assert!(f.starts_with("````\n"), "{f}");
        assert!(f.ends_with("\n````"), "{f}");
    }

    #[test]
    fn arguments_that_are_not_json_are_shown_verbatim() {
        assert!(fenced_json_or_text("{\"a\":1}", 1024).starts_with("```json"));
        assert!(fenced_json_or_text("not json", 1024).starts_with("```\nnot json"));
        assert_eq!(fenced_json_or_text("{}", 1024), "");
    }

    #[test]
    fn project_is_the_last_path_component() {
        assert_eq!(project_of("/Users/picard/src/enterprise"), "enterprise");
        assert_eq!(project_of("/Users/picard/src/enterprise/"), "enterprise");
        assert_eq!(project_of("/"), "/");
    }
}
