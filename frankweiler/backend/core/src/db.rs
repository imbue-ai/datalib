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

use crate::query::{extract_uuid_suffix, Field, ParsedQuery, RowType};
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

pub(crate) fn snippet(text: &str, needle: &str) -> String {
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

/// Look up the `qmd_path` for a conversation/thread, relative to the data
/// root. The grid populates this column at ingest time so the chat preview
/// can read the right QMD file directly — no globbing, no frontmatter scan.
/// Prefers the chat-level row (`Chat` / `Slack Thread`) but any row in the
/// conversation will have the same `qmd_path` value.
pub fn qmd_path_for_conversation(
    root: &Path,
    conversation_uuid: &str,
) -> Option<std::path::PathBuf> {
    let conn = open_mirror(root)?;
    let qmd_rel: String = conn
        .query_row(
            "SELECT qmd_path FROM grid_rows \
             WHERE conversation_uuid = ?1 AND qmd_path IS NOT NULL \
             ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
             LIMIT 1",
            [conversation_uuid],
            |r| r.get(0),
        )
        .ok()?;
    Some(root.join(qmd_rel))
}

/// Per-conversation header data fetched from `grid_rows`. The chat preview
/// renders the QMD body verbatim and pulls metadata for the header from
/// here — no QMD parsing.
#[derive(Debug, Default)]
pub struct ChatMeta {
    pub name: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
    pub when_ts: Option<String>,
    pub source_label: Option<String>,
    // Canonical web URL back to the provider, used for the page-level
    // "Open in …" button. For Slack rows `source_url` is null and we fall
    // back to `slack_link` (a slack:// deep link).
    pub source_url: Option<String>,
}

/// Fetch the chat-level row for a conversation. Returns `None` when the
/// mirror DB is missing or no chat row exists (e.g. mid-ingest).
pub fn chat_meta(root: &Path, conversation_uuid: &str) -> Option<ChatMeta> {
    let conn = open_mirror(root)?;
    conn.query_row(
        "SELECT conversation_name, account, project, channel, when_ts, source_label, \
                COALESCE(source_url, slack_link) \
         FROM grid_rows \
         WHERE conversation_uuid = ?1 \
         ORDER BY CASE WHEN kind IN ('Chat','Slack Thread') THEN 0 ELSE 1 END \
         LIMIT 1",
        [conversation_uuid],
        |r| {
            Ok(ChatMeta {
                name: r.get::<_, Option<String>>(0)?,
                account: r.get::<_, Option<String>>(1)?,
                project: r.get::<_, Option<String>>(2)?,
                channel: r.get::<_, Option<String>>(3)?,
                when_ts: r.get::<_, Option<String>>(4)?,
                source_label: r.get::<_, Option<String>>(5)?,
                source_url: r.get::<_, Option<String>>(6)?,
            })
        },
    )
    .ok()
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
                entire_chat, text, slack_link, notion_page_uuid \
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
            r.get::<_, Option<String>>(15)?.unwrap_or_default(), // notion_page_uuid
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
            channel,
            conversation_name,
            conversation_uuid,
            message_index,
            entire_chat,
            text,
            slack_link,
            notion_page_uuid,
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
            channel,
            slack_link,
            notion_page_uuid,
        });
    }
    rows
}

/// Map a query Field to the underlying `grid_rows` column it constrains,
/// or None for fields that aren't single-column equality filters
/// (Before/After are range, Type is a row-class classifier, Subj/Other
/// have no column yet).
fn column_for_field(f: &Field) -> Option<&'static str> {
    match f {
        Field::Source => Some("source_label"),
        Field::Kind => Some("kind"),
        Field::Channel => Some("channel"),
        // `convo:slug-uuid` filters on conversation_uuid (UUID-load-bearing).
        Field::Convo => Some("conversation_uuid"),
        Field::Author => Some("author"),
        Field::Account => Some("account"),
        Field::Project => Some("project"),
        Field::NotionPage => Some("notion_page_uuid"),
        Field::Before | Field::After | Field::Type | Field::Subj | Field::Other(_) => None,
    }
}

pub(crate) fn build_where(q: &ParsedQuery, needle: &str) -> (String, Vec<String>) {
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

    // Per-term AND filters. Each occurrence is its own clause — repeating
    // the same field with different values produces an empty result, which
    // matches the "keep only X then keep only Y" tree-zoom UX.
    for term in &q.terms {
        let Some(col) = column_for_field(&term.field) else {
            continue;
        };
        if term.negate {
            // Nullable columns: NULL would pass `col != ?` as unknown and
            // be dropped, which surprises users who didn't ask to exclude
            // unset values. Explicitly keep nulls.
            clauses.push(format!("({col} IS NULL OR {col} != ?)"));
        } else {
            clauses.push(format!("{col} = ?"));
        }
        // UUID-bearing fields accept Notion-shaped `slug-uuid` tokens; the
        // slug is non-load-bearing — strip it before binding.
        let bound = if term.field.is_uuid_bearing() {
            extract_uuid_suffix(&term.value).to_string()
        } else {
            term.value.clone()
        };
        params.push(bound);
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

    #[test]
    fn source_filter_keeps_only_matching_rows() {
        let conn = build_fixture();
        let rows = grid_rows_with_conn(&conn, &parse_query("source:Claude type:all"), 1000);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.source == "Claude"));
    }

    #[test]
    fn negated_source_excludes_matching_rows() {
        let conn = build_fixture();
        let rows = grid_rows_with_conn(&conn, &parse_query("-source:ChatGPT type:all"), 1000);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.source != "ChatGPT"));
    }

    #[test]
    fn same_field_repeated_with_different_values_is_empty() {
        // "Keep only Source=Claude, then keep only Source=ChatGPT" → AND →
        // empty. Tree-zoom semantics, deliberate.
        let conn = build_fixture();
        let rows = grid_rows_with_conn(
            &conn,
            &parse_query("source:Claude source:ChatGPT type:all"),
            1000,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn negated_filter_keeps_null_values() {
        // None of the fixture rows have a channel set; excluding a specific
        // channel must NOT drop rows whose channel is NULL.
        let conn = build_fixture();
        let baseline = grid_rows_with_conn(&conn, &parse_query("type:all"), 1000).len();
        let filtered =
            grid_rows_with_conn(&conn, &parse_query("-channel:announce type:all"), 1000).len();
        assert_eq!(baseline, filtered);
    }
}
