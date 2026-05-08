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
        "tool_use" | "tool_result" => "Tool Call",
        "thinking" => "LLM Response",
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

fn openai_kind_for_role(role: &str) -> &'static str {
    match role.to_ascii_lowercase().as_str() {
        "user" => "User Input",
        "assistant" => "LLM Response",
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
    let mut rows: Vec<SearchRow> = Vec::new();
    let needle = q.free_text.to_lowercase();
    let want_chats = matches!(q.resolved_type, RowType::Chat | RowType::All);
    let want_messages = matches!(q.resolved_type, RowType::Message | RowType::All);

    if want_chats {
        push_anthropic_chats(&conn, q, &needle, limit, &mut rows);
        if rows.len() < limit {
            push_openai_chats(&conn, q, &needle, limit, &mut rows);
        }
    }
    if want_messages && rows.len() < limit {
        push_anthropic_messages(&conn, q, &needle, limit, &mut rows);
        if rows.len() < limit {
            push_anthropic_blocks(&conn, q, &needle, limit, &mut rows);
        }
        if rows.len() < limit {
            push_openai_messages(&conn, q, &needle, limit, &mut rows);
        }
    }
    rows.truncate(limit);
    rows
}

fn build_filter_clause<'a>(
    q: &'a ParsedQuery,
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
    limit: usize,
    out: &mut Vec<SearchRow>,
) {
    let (where_sql, params) = build_filter_clause(
        q,
        "account_uuid",
        Some("project_uuid"),
        "IFNULL(updated_at, created_at)",
        &["name", "summary"],
        needle,
    );
    let sql = format!(
        "SELECT conversation_uuid, account_uuid, project_uuid, name, summary, \
                IFNULL(updated_at, created_at) AS when_ts \
         FROM anthropic_conversations{} ORDER BY when_ts DESC LIMIT ?",
        where_sql
    );
    let mut params: Vec<String> = params;
    params.push((limit - out.len()).to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else { return };
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
        let snip = if !summary.is_empty() { summary.clone() } else { name.clone() };
        out.push(SearchRow {
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
        if out.len() >= limit {
            return;
        }
    }
}

fn push_openai_chats(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    limit: usize,
    out: &mut Vec<SearchRow>,
) {
    let (where_sql, params) = build_filter_clause(
        q,
        "account_id",
        None,
        "IFNULL(update_time, create_time)",
        &["title"],
        needle,
    );
    let sql = format!(
        "SELECT conversation_id, account_id, title, default_model_slug, \
                IFNULL(update_time, create_time) AS when_ts \
         FROM openai_conversations{} ORDER BY when_ts DESC LIMIT ?",
        where_sql
    );
    let mut params = params;
    params.push((limit - out.len()).to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else { return };
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
        if out.len() >= limit {
            return;
        }
    }
}

fn push_anthropic_messages(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    limit: usize,
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
    let sql = format!(
        "SELECT m.message_uuid, m.conversation_uuid, m.sender, m.text, m.created_at, \
                c.name, c.project_uuid, c.account_uuid, \
                json_extract(c.raw_json, '$.model') AS conv_model \
         FROM anthropic_messages m JOIN anthropic_conversations c \
              ON m.conversation_uuid = c.conversation_uuid{} \
         ORDER BY m.created_at DESC LIMIT ?",
        where_sql
    );
    let mut params = params;
    params.push((limit - out.len()).to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else { return };
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
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (_mid, cuuid, sender, text, when, cname, project, account, model) = row;
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
            conversation_uuid: cuuid.clone(),
            message_index: Some(0),
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
        if out.len() >= limit {
            return;
        }
    }
}

fn push_anthropic_blocks(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    limit: usize,
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
    let sql = format!(
        "SELECT b.message_uuid, m.conversation_uuid, b.type, b.text, b.start_timestamp, \
                c.name, c.project_uuid, c.account_uuid, \
                json_extract(c.raw_json, '$.model') AS conv_model \
         FROM anthropic_content_blocks b \
              JOIN anthropic_messages m ON b.message_uuid = m.message_uuid \
              JOIN anthropic_conversations c ON m.conversation_uuid = c.conversation_uuid{} \
         ORDER BY b.start_timestamp DESC LIMIT ?",
        where_sql
    );
    let mut params = params;
    params.push((limit - out.len()).to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else { return };
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
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (_mid, cuuid, btype, text, when, cname, project, account, model) = row;
        let kind = anthropic_kind_for_block(&btype);
        let author = if !model.is_empty() { model.clone() } else { btype.clone() };
        let snippet_text = if text.is_empty() { btype.clone() } else { snippet(&text, needle) };
        out.push(SearchRow {
            conversation_uuid: cuuid.clone(),
            message_index: Some(0),
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
        if out.len() >= limit {
            return;
        }
    }
}

fn push_openai_messages(
    conn: &Connection,
    q: &ParsedQuery,
    needle: &str,
    limit: usize,
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
        "SELECT m.message_id, m.conversation_id, m.role, m.text, m.create_time, m.model_slug, \
                c.title, c.account_id \
         FROM openai_messages m JOIN openai_conversations c \
              ON m.conversation_id = c.conversation_id{} \
         ORDER BY m.create_time DESC LIMIT ?",
        where_sql
    );
    let mut params = params;
    params.push((limit - out.len()).to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else { return };
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
        ))
    });
    let Ok(it) = it else { return };
    for row in it.flatten() {
        let (_mid, cuuid, role, text, when, model, ctitle, account) = row;
        let kind = openai_kind_for_role(&role);
        let author = match kind {
            "User Input" => account.clone(),
            "LLM Response" => {
                if model.is_empty() {
                    role.clone()
                } else {
                    model.clone()
                }
            }
            _ => role.clone(),
        };
        out.push(SearchRow {
            conversation_uuid: cuuid.clone(),
            message_index: Some(0),
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
        if out.len() >= limit {
            return;
        }
    }
}
