//! Schema-driven grid query, backed by `<root>/mirror.sqlite`.
//!
//! The ingest pipeline writes a portable SQL dump of every Dolt table
//! into `<root>/mirror.sqlite` after each run. This module reads that
//! file and produces grid rows directly from the source data — no
//! markdown parsing, no inheritance heuristics. QMDs are the chat
//! viewer's source; the SQLite mirror is the grid's source.

use crate::query::{Field, ParsedQuery, RowType};
use crate::search::SearchRow;
use rusqlite::{params_from_iter, Connection, OpenFlags};
use std::path::Path;

/// Open the mirror DB read-only. Returns None if missing.
fn open_mirror(root: &Path) -> Option<Connection> {
    let path = root.join("mirror.sqlite");
    if !path.exists() {
        return None;
    }
    Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn anthropic_kind_for_block(block_type: &str) -> &'static str {
    match block_type {
        "thinking" => "LLM Thinking",
        "tool_use" | "tool_result" => "Tool Call",
        _ => "Tool Call",
    }
}

fn anthropic_kind_for_sender(sender: &str) -> &'static str {
    match sender.to_ascii_lowercase().as_str() {
        "human" | "user" => "User Input",
        "assistant" => "LLM Response",
        _ => "Tool Call",
    }
}

fn openai_kind_for_role_and_type(role: &str, content_type: &str) -> &'static str {
    match role.to_ascii_lowercase().as_str() {
        "user" => "User Input",
        "assistant" => match content_type {
            "thoughts" | "reasoning_recap" => "LLM Thinking",
            _ => "LLM Response",
        },
        "system" => "Tool Call",
        _ => "Tool Call",
    }
}

const SNIPPET_LEN: usize = 240;

fn snippet(text: &str, needle: &str) -> String {
    let trimmed = if needle.is_empty() {
        first_chars(text, SNIPPET_LEN)
    } else {
        let lower = text.to_lowercase();
        match lower.find(needle) {
            Some(pos) => {
                let radius = SNIPPET_LEN / 2;
                let start = text[..pos]
                    .char_indices()
                    .rev()
                    .nth(radius)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                let end_byte = pos + needle.len();
                let end = text[end_byte..]
                    .char_indices()
                    .nth(radius)
                    .map(|(i, _)| end_byte + i)
                    .unwrap_or(text.len());
                let mut out = String::new();
                if start > 0 {
                    out.push('…');
                }
                out.push_str(&text[start..end]);
                if end < text.len() {
                    out.push('…');
                }
                out
            }
            None => first_chars(text, SNIPPET_LEN),
        }
    };
    trimmed.replace('\n', " ")
}

fn first_chars(s: &str, n: usize) -> String {
    let end = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    let mut out = s[..end].to_string();
    if end < s.len() {
        out.push('…');
    }
    out
}

/// Anthropic messages get their model from the parent conversation
/// (`anthropic_conversations.raw_json.model`); per-message model is not
/// stored on the export. OpenAI carries `model_slug` on the message.
pub fn grid_rows(root: &Path, q: &ParsedQuery, limit: usize) -> Vec<SearchRow> {
    let Some(conn) = open_mirror(root) else {
        return Vec::new();
    };
    grid_rows_with_conn(&conn, q, limit)
}

pub fn grid_rows_with_conn(conn: &Connection, q: &ParsedQuery, limit: usize) -> Vec<SearchRow> {
    let mut rows: Vec<SearchRow> = Vec::new();
    let needle = q.free_text.to_lowercase();
    let want_chats = matches!(q.resolved_type, RowType::Chat | RowType::All);
    let want_messages = matches!(q.resolved_type, RowType::Message | RowType::All);

    // Data is small; fetch everything from each source, then sort globally.
    // Per-section LIMITs caused later sections (e.g. openai messages) to be
    // starved of budget when earlier sections filled the cap.
    if want_chats {
        push_anthropic_chats(conn, q, &needle, &mut rows);
        push_openai_chats(conn, q, &needle, &mut rows);
    }
    if want_messages {
        push_anthropic_messages(conn, q, &needle, &mut rows);
        push_anthropic_blocks(conn, q, &needle, &mut rows);
        push_openai_messages(conn, q, &needle, &mut rows);
    }
    // Global ascending sort by time. Chat rows tie-break ahead of messages
    // so each conversation's Chat row precedes its own messages when their
    // timestamps coincide.
    rows.sort_by(|a, b| {
        a.when
            .cmp(&b.when)
            .then_with(|| (a.kind != "Chat").cmp(&(b.kind != "Chat")))
    });
    rows.truncate(limit);
    rows
}

fn build_filter_clause(
    q: &ParsedQuery,
    account_col: &str,
    project_col: Option<&str>,
    when_col: &str,
    text_cols: &[&str],
    needle: &str,
) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some(vals) = q.filters.get(&Field::Account) {
        if !vals.is_empty() {
            let qs = vec!["?"; vals.len()].join(",");
            clauses.push(format!("{} IN ({})", account_col, qs));
            for v in vals {
                params.push(v.clone());
            }
        }
    }
    if let (Some(pcol), Some(vals)) = (project_col, q.filters.get(&Field::Project)) {
        if !vals.is_empty() {
            let qs = vec!["?"; vals.len()].join(",");
            clauses.push(format!("{} IN ({})", pcol, qs));
            for v in vals {
                params.push(v.clone());
            }
        }
    }
    if let Some(vals) = q.filters.get(&Field::Before) {
        if let Some(v) = vals.first() {
            clauses.push(format!("{} < ?", when_col));
            params.push(v.clone());
        }
    }
    if let Some(vals) = q.filters.get(&Field::After) {
        if let Some(v) = vals.first() {
            clauses.push(format!("{} > ?", when_col));
            params.push(v.clone());
        }
    }
    if !needle.is_empty() && !text_cols.is_empty() {
        let like = format!("%{}%", needle);
        let parts: Vec<String> = text_cols
            .iter()
            .map(|c| format!("LOWER(IFNULL({},'')) LIKE ?", c))
            .collect();
        clauses.push(format!("({})", parts.join(" OR ")));
        for _ in text_cols {
            params.push(like.clone());
        }
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

fn push_anthropic_chats(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    out: &mut Vec<SearchRow>,
) {
    // Use created_at as the chat's effective time so the Chat row sorts
    // ahead of its own messages under a global ascending time order.
    let (where_sql, params) = build_filter_clause(
        q,
        "account_uuid",
        Some("project_uuid"),
        "IFNULL(created_at, updated_at)",
        &["name", "summary"],
        needle,
    );
    let sql = format!(
        "SELECT conversation_uuid, account_uuid, project_uuid, name, summary, \
                IFNULL(created_at, updated_at) AS when_ts \
         FROM anthropic_conversations{}",
        where_sql
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let it = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (uuid, account, project, name, summary, when) = row;
        let snip = if !summary.is_empty() {
            summary.clone()
        } else {
            name.clone()
        };
        out.push(SearchRow {
            uuid: uuid.clone(),
            conversation_uuid: uuid.clone(),
            message_index: None,
            snippet: snip,
            sender: String::new(),
            when,
            conversation_name: name,
            project,
            account,
            entire_chat: format!("/chat/{}", uuid),
            source: "Claude".into(),
            kind: "Chat".into(),
            author: String::new(),
        });
    }
}

fn push_openai_chats(conn: &Connection, q: &ParsedQuery, needle: &str, out: &mut Vec<SearchRow>) {
    let (where_sql, params) = build_filter_clause(
        q,
        "account_id",
        None,
        "IFNULL(create_time, update_time)",
        &["title"],
        needle,
    );
    let sql = format!(
        "SELECT conversation_id, account_id, title, default_model_slug, \
                IFNULL(create_time, update_time) AS when_ts \
         FROM openai_conversations{}",
        where_sql
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let it = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (uuid, account, title, _model, when) = row;
        out.push(SearchRow {
            uuid: uuid.clone(),
            conversation_uuid: uuid.clone(),
            message_index: None,
            snippet: title.clone(),
            sender: String::new(),
            when,
            conversation_name: title,
            project: String::new(),
            account,
            entire_chat: format!("/chat/{}", uuid),
            source: "ChatGPT".into(),
            kind: "Chat".into(),
            author: String::new(),
        });
    }
}

fn push_anthropic_messages(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    out: &mut Vec<SearchRow>,
) {
    let (where_sql, params) = build_filter_clause(
        q,
        "c.account_uuid",
        Some("c.project_uuid"),
        "m.created_at",
        &["m.text"],
        needle,
    );
    // Per-conversation `msg_idx` mirrors the QMD render order (created_at,
    // tie-break by message_uuid) so the chat preview pane can use it as the
    // index into `chat.messages` for `scrollIntoView('#m-idx-N')`.
    let sql = format!(
        "WITH m AS (\
            SELECT message_uuid, conversation_uuid, sender, text, created_at, \
                   ROW_NUMBER() OVER (PARTITION BY conversation_uuid \
                                      ORDER BY created_at, message_uuid) - 1 AS msg_idx \
            FROM anthropic_messages\
         ) \
         SELECT m.message_uuid, m.conversation_uuid, m.sender, m.text, m.created_at, \
                c.name, c.project_uuid, c.account_uuid, \
                json_extract(c.raw_json, '$.model') AS conv_model, m.msg_idx \
         FROM m JOIN anthropic_conversations c \
              ON m.conversation_uuid = c.conversation_uuid{}",
        where_sql
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let it = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            r.get::<_, i64>(9)?,
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (mid, cuuid, sender, text, when, cname, project, account, model, msg_idx) = row;
        let kind = anthropic_kind_for_sender(&sender);
        let author = match kind {
            "User Input" => account.clone(),
            "LLM Response" => {
                if model.is_empty() {
                    sender.clone()
                } else {
                    model.clone()
                }
            }
            _ => sender.clone(),
        };
        out.push(SearchRow {
            uuid: mid.clone(),
            conversation_uuid: cuuid.clone(),
            message_index: Some(msg_idx as usize),
            snippet: snippet(&text, needle),
            sender: sender.clone(),
            when,
            conversation_name: cname,
            project,
            account,
            entire_chat: format!("/chat/{}", cuuid),
            source: "Claude".into(),
            kind: kind.into(),
            author,
        });
    }
}

fn push_anthropic_blocks(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    out: &mut Vec<SearchRow>,
) {
    let (where_sql, params) = build_filter_clause(
        q,
        "c.account_uuid",
        Some("c.project_uuid"),
        "b.start_timestamp",
        &["b.text"],
        needle,
    );
    // Tool-use, tool-result, and thinking blocks are first-class grid rows.
    let type_clause = "b.type IN ('tool_use','tool_result','thinking')";
    let where_sql = if where_sql.is_empty() {
        format!(" WHERE {}", type_clause)
    } else {
        format!("{} AND {}", where_sql, type_clause)
    };
    // Block rows scroll to their *parent message*, since blocks render
    // inline inside their parent in the QMD/preview output.
    let sql = format!(
        "WITH m AS (\
            SELECT message_uuid, conversation_uuid, created_at, \
                   ROW_NUMBER() OVER (PARTITION BY conversation_uuid \
                                      ORDER BY created_at, message_uuid) - 1 AS msg_idx \
            FROM anthropic_messages\
         ) \
         SELECT b.message_uuid, m.conversation_uuid, b.type, \
                COALESCE(NULLIF(b.text, ''), json_extract(b.raw_json, '$.thinking')) AS btext, \
                b.start_timestamp, \
                c.name, c.project_uuid, c.account_uuid, \
                json_extract(c.raw_json, '$.model') AS conv_model, m.msg_idx, \
                m.created_at, b.block_index \
         FROM anthropic_content_blocks b \
              JOIN m ON b.message_uuid = m.message_uuid \
              JOIN anthropic_conversations c ON m.conversation_uuid = c.conversation_uuid{} \
         ORDER BY m.conversation_uuid, m.created_at, b.message_uuid, b.block_index",
        where_sql
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let it = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            r.get::<_, i64>(9)?,
            r.get::<_, Option<String>>(10)?.unwrap_or_default(),
            r.get::<_, i64>(11)?,
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (
            mid,
            cuuid,
            btype,
            text,
            when,
            cname,
            project,
            account,
            model,
            msg_idx,
            msg_created,
            block_index,
        ) = row;
        let kind = anthropic_kind_for_block(&btype);
        let author = if !model.is_empty() {
            model.clone()
        } else {
            btype.clone()
        };
        let snippet_text = if text.is_empty() {
            btype.clone()
        } else {
            snippet(&text, needle)
        };
        // Synthesize a timestamp when the block has none: parent message's
        // created_at plus a microsecond bump per block_index, so blocks
        // within a message keep their export-order ordering.
        let when = if when.is_empty() && !msg_created.is_empty() {
            bump_micros(&msg_created, block_index + 1)
        } else {
            when
        };
        out.push(SearchRow {
            uuid: format!("{}:{}", mid, block_index),
            conversation_uuid: cuuid.clone(),
            message_index: Some(msg_idx as usize),
            snippet: snippet_text,
            sender: btype.clone(),
            when,
            conversation_name: cname,
            project,
            account,
            entire_chat: format!("/chat/{}", cuuid),
            source: "Claude".into(),
            kind: kind.into(),
            author,
        });
    }
}

/// Add `n` microseconds to an ISO-8601 timestamp string, preserving the
/// `+00:00` / `Z` suffix. Falls back to returning the input unchanged if
/// the format isn't recognized — synthetic ordering is best-effort.
fn bump_micros(ts: &str, n: i64) -> String {
    use chrono::{DateTime, FixedOffset};
    match DateTime::<FixedOffset>::parse_from_rfc3339(ts) {
        Ok(dt) => {
            let bumped = dt + chrono::Duration::microseconds(n);
            // Match the export format ("...+00:00") rather than the default "...Z".
            bumped.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string()
        }
        Err(_) => ts.to_string(),
    }
}

fn push_openai_messages(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    out: &mut Vec<SearchRow>,
) {
    let (where_sql, params) = build_filter_clause(
        q,
        "c.account_id",
        None,
        "m.create_time",
        &["m.text"],
        needle,
    );
    let sql = format!(
        "WITH m AS (\
            SELECT message_id, conversation_id, role, content_type, text, create_time, model_slug, \
                   ROW_NUMBER() OVER (PARTITION BY conversation_id \
                                      ORDER BY create_time, message_id) - 1 AS msg_idx \
            FROM openai_messages\
         ) \
         SELECT m.message_id, m.conversation_id, m.role, m.text, m.create_time, m.model_slug, \
                c.title, c.account_id, m.msg_idx, \
                IFNULL(c.create_time, c.update_time) AS conv_time, \
                IFNULL(m.content_type, '') AS content_type \
         FROM m JOIN openai_conversations c \
              ON m.conversation_id = c.conversation_id{}",
        where_sql
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return;
    };
    let it = stmt.query_map(params_from_iter(params.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),
            r.get::<_, i64>(8)?,
            r.get::<_, Option<String>>(9)?.unwrap_or_default(),
            r.get::<_, Option<String>>(10)?.unwrap_or_default(),
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (
            mid,
            cuuid,
            role,
            text,
            when,
            model,
            ctitle,
            account,
            msg_idx,
            conv_time,
            content_type,
        ) = row;
        // Persona/system messages and other rows can be missing create_time;
        // fall back to the parent conversation's create_time plus a per-row
        // microsecond bump so ordering within the conversation stays stable.
        let when = if when.is_empty() && !conv_time.is_empty() {
            bump_micros(&conv_time, msg_idx + 1)
        } else {
            when
        };
        let kind = openai_kind_for_role_and_type(&role, &content_type);
        let author = match kind {
            "User Input" => account.clone(),
            "LLM Response" | "LLM Thinking" => {
                if model.is_empty() {
                    role.clone()
                } else {
                    model.clone()
                }
            }
            _ => role.clone(),
        };
        out.push(SearchRow {
            uuid: mid.clone(),
            conversation_uuid: cuuid.clone(),
            message_index: Some(msg_idx as usize),
            snippet: snippet(&text, needle),
            sender: role.clone(),
            when,
            conversation_name: ctitle,
            project: String::new(),
            account,
            entire_chat: format!("/chat/{}", cuuid),
            source: "ChatGPT".into(),
            kind: kind.into(),
            author,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;
    use rusqlite::Connection;

    /// Minimal schema mirror — just the columns the grid query reads.
    /// Keep it in sync with `src/ingest/providers/*/schema.py`.
    fn build_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE anthropic_accounts (
                account_uuid TEXT PRIMARY KEY, email TEXT, full_name TEXT,
                raw_json TEXT, source TEXT, first_seen_at TEXT, last_seen_at TEXT
            );
            CREATE TABLE anthropic_projects (
                account_uuid TEXT, project_uuid TEXT PRIMARY KEY, name TEXT,
                description TEXT, is_starter INT, created_at TEXT, updated_at TEXT,
                raw_json TEXT, source TEXT, last_seen_at TEXT
            );
            CREATE TABLE anthropic_conversations (
                account_uuid TEXT, conversation_uuid TEXT PRIMARY KEY,
                project_uuid TEXT, name TEXT, summary TEXT,
                created_at TEXT, updated_at TEXT, raw_json TEXT,
                source TEXT, last_seen_at TEXT
            );
            CREATE TABLE anthropic_messages (
                conversation_uuid TEXT, message_uuid TEXT PRIMARY KEY,
                parent_message_uuid TEXT, sender TEXT, text TEXT,
                created_at TEXT, updated_at TEXT, raw_json TEXT,
                source TEXT, last_seen_at TEXT
            );
            CREATE TABLE anthropic_content_blocks (
                message_uuid TEXT, block_index INT, type TEXT, text TEXT,
                start_timestamp TEXT, stop_timestamp TEXT, raw_json TEXT,
                source TEXT, PRIMARY KEY (message_uuid, block_index)
            );
            CREATE TABLE anthropic_attachments (
                message_uuid TEXT, attachment_index INT, kind TEXT,
                raw_json TEXT, source TEXT,
                PRIMARY KEY (message_uuid, attachment_index, kind)
            );
            CREATE TABLE openai_accounts (
                account_id TEXT PRIMARY KEY, email TEXT, name TEXT,
                raw_json TEXT, source TEXT, first_seen_at TEXT, last_seen_at TEXT
            );
            CREATE TABLE openai_conversations (
                account_id TEXT, conversation_id TEXT PRIMARY KEY, title TEXT,
                create_time TEXT, update_time TEXT, current_node TEXT,
                default_model_slug TEXT, gizmo_id TEXT, gizmo_type TEXT,
                is_archived INT, is_starred INT, raw_json TEXT,
                source TEXT, last_seen_at TEXT
            );
            CREATE TABLE openai_messages (
                conversation_id TEXT, message_id TEXT PRIMARY KEY,
                parent_id TEXT, role TEXT, recipient TEXT, channel TEXT,
                content_type TEXT, text TEXT, status TEXT, end_turn INT,
                weight REAL, model_slug TEXT, create_time TEXT,
                update_time TEXT, raw_json TEXT, source TEXT, last_seen_at TEXT
            );
            CREATE TABLE openai_content_parts (
                message_id TEXT, part_index INT, kind TEXT, language TEXT,
                text TEXT, raw_json TEXT, source TEXT,
                PRIMARY KEY (message_id, part_index)
            );
            ",
        )
        .unwrap();

        // Many anthropic chats and messages so they crowd a small overall budget.
        // 5 chats × 4 messages each = 5 chat rows + 20 message rows.
        for i in 0..5 {
            let cuuid = format!("a-conv-{i:02}");
            conn.execute(
                "INSERT INTO anthropic_conversations (account_uuid, conversation_uuid, name, \
                 created_at, updated_at, raw_json, source, last_seen_at) \
                 VALUES (?, ?, ?, ?, ?, ?, 'export', ?)",
                rusqlite::params![
                    "acct-a",
                    &cuuid,
                    &format!("Anthropic chat {i}"),
                    &format!("2026-04-{:02}T10:00:00Z", i + 1),
                    &format!("2026-04-{:02}T10:30:00Z", i + 1),
                    "{\"model\":\"claude-opus-4-7\"}",
                    &format!("2026-04-{:02}T10:30:00Z", i + 1),
                ],
            )
            .unwrap();
            for j in 0..4 {
                conn.execute(
                    "INSERT INTO anthropic_messages (conversation_uuid, message_uuid, sender, \
                     text, created_at, updated_at, raw_json, source, last_seen_at) \
                     VALUES (?, ?, ?, ?, ?, ?, '{}', 'export', ?)",
                    rusqlite::params![
                        &cuuid,
                        &format!("a-msg-{i:02}-{j:02}"),
                        if j % 2 == 0 { "human" } else { "assistant" },
                        &format!("anthropic msg {i}-{j} body text"),
                        &format!("2026-04-{:02}T10:{:02}:00Z", i + 1, j),
                        &format!("2026-04-{:02}T10:{:02}:00Z", i + 1, j),
                        &format!("2026-04-{:02}T10:{:02}:00Z", i + 1, j),
                    ],
                )
                .unwrap();
            }
        }
        // One March-2025 ChatGPT conversation with several messages.
        conn.execute(
            "INSERT INTO openai_conversations (account_id, conversation_id, title, create_time, \
             update_time, raw_json, source, last_seen_at) \
             VALUES (?, ?, ?, ?, ?, '{}', 'api', ?)",
            rusqlite::params![
                "acct-o",
                "o-conv-01",
                "Defaults in Program Design",
                "2025-03-31T22:02:49Z",
                "2025-03-31T22:02:56Z",
                "2025-03-31T22:02:56Z",
            ],
        )
        .unwrap();
        for (j, role) in ["user", "assistant", "user", "assistant"]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO openai_messages (conversation_id, message_id, role, text, \
                 create_time, model_slug, raw_json, source, last_seen_at) \
                 VALUES (?, ?, ?, ?, ?, 'gpt-5', '{}', 'api', ?)",
                rusqlite::params![
                    "o-conv-01",
                    &format!("o-msg-{j:02}"),
                    role,
                    &format!("openai msg {j} body text"),
                    &format!("2025-03-31T22:02:5{}Z", j),
                    &format!("2025-03-31T22:02:5{}Z", j),
                ],
            )
            .unwrap();
        }
        conn
    }

    /// Bug #3 contract: results are globally sorted by time ascending,
    /// with each Chat row appearing immediately before its own messages.
    /// The current implementation enumerates source-by-source (anthropic
    /// chats, openai chats, anthropic messages, …) so the older March-
    /// 2025 ChatGPT chat ends up *after* the April-2026 anthropic chats,
    /// and its messages may be cut off entirely. This test pins the
    /// desired behavior; it should fail until the bug is fixed.
    #[test]
    fn rows_sorted_by_time_ascending_with_chat_before_its_messages() {
        let conn = build_fixture();
        let rows = grid_rows_with_conn(&conn, &parse_query(""), 1000);

        // Every ChatGPT message must surface (no per-source budget eviction).
        let openai_msg_count = rows
            .iter()
            .filter(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .count();
        assert_eq!(
            openai_msg_count,
            4,
            "expected all 4 ChatGPT messages; rows: {:?}",
            rows.iter()
                .map(|r| (r.source.clone(), r.kind.clone(), r.when.clone()))
                .collect::<Vec<_>>()
        );

        // Globally ascending by `when`.
        let times: Vec<&str> = rows.iter().map(|r| r.when.as_str()).collect();
        for w in times.windows(2) {
            assert!(
                w[0] <= w[1],
                "rows not sorted ascending by when: {:?}",
                times
            );
        }

        // The oldest entries belong to the March-2025 ChatGPT conversation,
        // and its Chat row must come before any of its messages.
        let first_chatgpt_chat = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind == "Chat")
            .expect("no ChatGPT Chat row found");
        let first_chatgpt_msg = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .expect("no ChatGPT message rows found");
        assert!(
            first_chatgpt_chat < first_chatgpt_msg,
            "ChatGPT Chat row must precede its messages (chat at {}, msg at {})",
            first_chatgpt_chat,
            first_chatgpt_msg,
        );
    }
}
