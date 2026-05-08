//! Schema-driven grid query, backed by `<root>/mirror.sqlite`.
//!
//! The ingest pipeline writes a portable SQL dump of every Dolt table
//! into `<root>/mirror.sqlite` after each run, including the `grid_rows`
//! union table — a single denormalized row per displayable entity (chat,
//! message, content block, slack message, ...). This module fires one
//! SELECT against `grid_rows` and renders directly. Per-provider tables
//! remain the authoritative store; `grid_rows` is the projection the grid
//! reads. Schema (column names, types, per-provider mappings) lives in
//! `//schemas/grid_rows.schema.json`.
//!
//! See `docs/grid_rows.md` for the architecture overview.

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

pub fn grid_rows(root: &Path, q: &ParsedQuery, limit: usize) -> Vec<SearchRow> {
    let Some(conn) = open_mirror(root) else {
        return Vec::new();
    };
    grid_rows_with_conn(&conn, q, limit)
}

pub fn grid_rows_with_conn(conn: &Connection, q: &ParsedQuery, limit: usize) -> Vec<SearchRow> {
    let needle = q.free_text.to_lowercase();
    let (where_sql, params) = build_where(q, &needle);
    // Global ascending sort by time. Chat rows tie-break ahead of messages
    // so each conversation's Chat row precedes its own messages when their
    // timestamps coincide.
    let sql = format!(
        "SELECT uuid, provider, kind, source_label, when_ts, author, account, project, \
                channel, conversation_name, conversation_uuid, message_index, \
                entire_chat, text, slack_link \
         FROM grid_rows{} \
         ORDER BY when_ts ASC, CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END, uuid \
         LIMIT ?",
        where_sql
    );
    let mut bind: Vec<String> = params;
    bind.push(limit.to_string());
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let it = stmt.query_map(params_from_iter(bind.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,                              // uuid
            r.get::<_, String>(1)?,                              // provider
            r.get::<_, String>(2)?,                              // kind
            r.get::<_, String>(3)?,                              // source_label
            r.get::<_, String>(4)?,                              // when_ts
            r.get::<_, Option<String>>(5)?.unwrap_or_default(),  // author
            r.get::<_, Option<String>>(6)?.unwrap_or_default(),  // account
            r.get::<_, Option<String>>(7)?.unwrap_or_default(),  // project
            r.get::<_, Option<String>>(8)?.unwrap_or_default(),  // channel
            r.get::<_, Option<String>>(9)?.unwrap_or_default(),  // conversation_name
            r.get::<_, String>(10)?,                             // conversation_uuid
            r.get::<_, Option<i64>>(11)?,                        // message_index
            r.get::<_, String>(12)?,                             // entire_chat
            r.get::<_, String>(13)?,                             // text
            r.get::<_, Option<String>>(14)?.unwrap_or_default(), // slack_link
        ))
    });
    let Ok(it) = it else { return Vec::new() };
    let mut rows: Vec<SearchRow> = Vec::new();
    for row in it.flatten() {
        let (
            uuid,
            _provider,
            kind,
            source_label,
            when_ts,
            author,
            account,
            project,
            _channel,
            conversation_name,
            conversation_uuid,
            message_index,
            entire_chat,
            text,
            _slack_link,
        ) = row;
        let snip = if kind == "Chat" {
            text.clone()
        } else {
            snippet(&text, &needle)
        };
        rows.push(SearchRow {
            uuid,
            conversation_uuid,
            message_index: message_index.map(|n| n as usize),
            snippet: snip,
            // `sender` is no longer carried per-row in grid_rows; map from author.
            sender: author.clone(),
            when: when_ts,
            conversation_name,
            project,
            account,
            entire_chat,
            source: source_label,
            kind,
            author,
        });
    }
    rows
}

fn build_where(q: &ParsedQuery, needle: &str) -> (String, Vec<String>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    match q.resolved_type {
        RowType::Chat => {
            clauses.push("kind IN ('Chat','Slack Thread')".into());
        }
        RowType::Message => {
            clauses.push("kind NOT IN ('Chat','Slack Thread')".into());
        }
        RowType::All => {}
    }

    if let Some(vals) = q.filters.get(&Field::Account) {
        if !vals.is_empty() {
            let qs = vec!["?"; vals.len()].join(",");
            clauses.push(format!("account IN ({})", qs));
            for v in vals {
                params.push(v.clone());
            }
        }
    }
    if let Some(vals) = q.filters.get(&Field::Project) {
        if !vals.is_empty() {
            let qs = vec!["?"; vals.len()].join(",");
            clauses.push(format!("project IN ({})", qs));
            for v in vals {
                params.push(v.clone());
            }
        }
    }
    if let Some(vals) = q.filters.get(&Field::Before) {
        if let Some(v) = vals.first() {
            clauses.push("when_ts < ?".into());
            params.push(v.clone());
        }
    }
    if let Some(vals) = q.filters.get(&Field::After) {
        if let Some(v) = vals.first() {
            clauses.push("when_ts > ?".into());
            params.push(v.clone());
        }
    }
    if !needle.is_empty() {
        clauses.push("LOWER(text) LIKE ?".into());
        params.push(format!("%{}%", needle));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse_query;
    use frankweiler_schema::grid_rows::DDL as GRID_DDL;
    use rusqlite::Connection;

    fn build_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        for (_table, ddl) in GRID_DDL {
            conn.execute_batch(ddl).unwrap();
        }
        // 5 anthropic chats × (1 chat row + 4 message rows).
        for i in 0..5u32 {
            let cuuid = format!("a-conv-{i:02}");
            let when = format!("2026-04-{:02}T10:00:00+00:00", i + 1);
            conn.execute(
                "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                 author, account, project, channel, conversation_name, conversation_uuid, \
                 message_index, entire_chat, text, slack_link) \
                 VALUES (?, 'anthropic', 'Chat', 'Claude', ?, NULL, 'acct-a', NULL, NULL, ?, ?, NULL, ?, ?, NULL)",
                rusqlite::params![
                    &cuuid,
                    &when,
                    &format!("Anthropic chat {i}"),
                    &cuuid,
                    &format!("/chat/{cuuid}"),
                    &format!("summary {i}"),
                ],
            )
            .unwrap();
            for j in 0..4u32 {
                let mwhen = format!("2026-04-{:02}T10:0{}:00+00:00", i + 1, j);
                let kind = if j % 2 == 0 {
                    "User Input"
                } else {
                    "LLM Response"
                };
                conn.execute(
                    "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                     author, account, project, channel, conversation_name, conversation_uuid, \
                     message_index, entire_chat, text, slack_link) \
                     VALUES (?, 'anthropic', ?, 'Claude', ?, ?, 'acct-a', NULL, NULL, ?, ?, ?, ?, ?, NULL)",
                    rusqlite::params![
                        &format!("a-msg-{i:02}-{j:02}"),
                        kind,
                        &mwhen,
                        if kind == "User Input" { "acct-a" } else { "claude-opus-4-7" },
                        &format!("Anthropic chat {i}"),
                        &cuuid,
                        j as i64,
                        &format!("/chat/{cuuid}"),
                        &format!("anthropic msg {i}-{j} body text"),
                    ],
                )
                .unwrap();
            }
        }
        // One March-2025 ChatGPT conversation with 4 messages.
        conn.execute(
            "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
             author, account, project, channel, conversation_name, conversation_uuid, \
             message_index, entire_chat, text, slack_link) \
             VALUES ('o-conv-01', 'openai', 'Chat', 'ChatGPT', '2025-03-31T22:02:49+00:00', \
                     NULL, 'acct-o', NULL, NULL, 'Defaults in Program Design', 'o-conv-01', \
                     NULL, '/chat/o-conv-01', 'Defaults in Program Design', NULL)",
            [],
        )
        .unwrap();
        for (j, role) in ["user", "assistant", "user", "assistant"]
            .iter()
            .enumerate()
        {
            let kind = if *role == "user" {
                "User Input"
            } else {
                "LLM Response"
            };
            conn.execute(
                "INSERT INTO grid_rows (uuid, provider, kind, source_label, when_ts, \
                 author, account, project, channel, conversation_name, conversation_uuid, \
                 message_index, entire_chat, text, slack_link) \
                 VALUES (?, 'openai', ?, 'ChatGPT', ?, ?, 'acct-o', NULL, NULL, \
                         'Defaults in Program Design', 'o-conv-01', ?, '/chat/o-conv-01', ?, NULL)",
                rusqlite::params![
                    &format!("o-msg-{j:02}"),
                    kind,
                    &format!("2025-03-31T22:02:5{j}+00:00"),
                    if kind == "User Input" {
                        "acct-o"
                    } else {
                        "gpt-5"
                    },
                    j as i64,
                    &format!("openai msg {j} body text"),
                ],
            )
            .unwrap();
        }
        conn
    }

    /// Bug #3 contract: results are globally sorted by time ascending, with
    /// each Chat row appearing immediately before its own messages.
    #[test]
    fn rows_sorted_by_time_ascending_with_chat_before_its_messages() {
        let conn = build_fixture();
        let rows = grid_rows_with_conn(&conn, &parse_query(""), 1000);

        let openai_msg_count = rows
            .iter()
            .filter(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .count();
        assert_eq!(openai_msg_count, 4);

        let times: Vec<&str> = rows.iter().map(|r| r.when.as_str()).collect();
        for w in times.windows(2) {
            assert!(
                w[0] <= w[1],
                "rows not sorted ascending by when: {:?}",
                times
            );
        }

        let first_chatgpt_chat = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind == "Chat")
            .expect("no ChatGPT Chat row found");
        let first_chatgpt_msg = rows
            .iter()
            .position(|r| r.source == "ChatGPT" && r.kind != "Chat")
            .expect("no ChatGPT message rows found");
        assert!(first_chatgpt_chat < first_chatgpt_msg);
    }
}
