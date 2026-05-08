//! In-memory search over parsed QMD conversations.
//!
//! v0: linear scan, case-insensitive substring match, structured filters
//! enforced against frontmatter. Fast enough for personal-scale corpora;
//! revisit with a real index (tantivy) once the corpus warrants it.

use crate::qmd::Conversation;
use crate::query::{Field, ParsedQuery, RowType};
use serde::Serialize;

const SNIPPET_RADIUS: usize = 80;

#[derive(Debug, Clone, Serialize)]
pub struct SearchRow {
    /// Stable per-row identifier:
    ///   chat row    → conversation_uuid
    ///   message row → message_uuid (DB) or `{conv_uuid}#m{idx}` (QMD fallback)
    ///   block row   → `{message_uuid}:{block_index}`
    pub uuid: String,
    pub conversation_uuid: String,
    pub message_index: Option<usize>,
    pub snippet: String,
    pub sender: String,
    pub when: String,
    pub conversation_name: String,
    pub project: String,
    pub account: String,
    pub entire_chat: String,
    pub source: String,
    pub kind: String,
    pub author: String,
}

fn source_label(provider: &str) -> String {
    match provider.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => "Claude".into(),
        "openai" | "chatgpt" => "ChatGPT".into(),
        "" => String::new(),
        other => other.to_string(),
    }
}

fn message_kind(sender: &str) -> &'static str {
    match sender.to_ascii_lowercase().as_str() {
        "human" | "user" => "User Input",
        "assistant" | "model" | "claude" | "chatgpt" | "gpt" => "LLM Response",
        _ => "Tool Call",
    }
}

pub fn search(convs: &[Conversation], q: &ParsedQuery, limit: usize) -> Vec<SearchRow> {
    let needle = q.free_text.to_lowercase();
    let mut rows: Vec<SearchRow> = Vec::new();
    for c in convs {
        if !passes_frontmatter_filters(c, q) {
            continue;
        }
        match q.resolved_type {
            RowType::Chat => {
                if needle.is_empty() || conversation_matches(c, &needle) {
                    rows.push(chat_row(c, &needle));
                }
            }
            RowType::All => {
                rows.push(chat_row(c, &needle));
                if rows.len() >= limit {
                    break;
                }
                for (i, m) in c.messages.iter().enumerate() {
                    if !needle.is_empty() && !m.text.to_lowercase().contains(&needle) {
                        continue;
                    }
                    rows.push(message_row(c, i, &needle));
                    if rows.len() >= limit {
                        return rows;
                    }
                }
            }
            RowType::Message => {
                let author_filter = q.filters.get(&Field::Author).cloned().unwrap_or_default();
                for (i, m) in c.messages.iter().enumerate() {
                    if !author_filter.is_empty()
                        && !author_filter
                            .iter()
                            .any(|a| m.sender.eq_ignore_ascii_case(a))
                    {
                        continue;
                    }
                    if !needle.is_empty() && !m.text.to_lowercase().contains(&needle) {
                        continue;
                    }
                    rows.push(message_row(c, i, &needle));
                    if rows.len() >= limit {
                        return rows;
                    }
                }
            }
        }
        if rows.len() >= limit {
            break;
        }
    }
    rows.truncate(limit);
    rows
}

fn passes_frontmatter_filters(c: &Conversation, q: &ParsedQuery) -> bool {
    for (field, vals) in &q.filters {
        match field {
            Field::Account => {
                if !any_eq(vals, c.frontmatter.account_uuid.as_deref()) {
                    return false;
                }
            }
            Field::Project => {
                if !any_eq(vals, c.frontmatter.project_uuid.as_deref()) {
                    return false;
                }
            }
            Field::Subj => {
                let n = c.frontmatter.name.as_deref().unwrap_or("").to_lowercase();
                if !vals.iter().any(|v| n.contains(&v.to_lowercase())) {
                    return false;
                }
            }
            Field::Before => {
                let ts = c.frontmatter.created_at.as_deref().unwrap_or("");
                if !vals.iter().any(|v| ts < v.as_str()) {
                    return false;
                }
            }
            Field::After => {
                let ts = c.frontmatter.created_at.as_deref().unwrap_or("");
                if !vals.iter().any(|v| ts > v.as_str()) {
                    return false;
                }
            }
            // Author is enforced at message level for RowType::Message; ignore for chats.
            // Type is the resolver, not a filter.
            // Other fields are accepted but ignored at this layer.
            _ => {}
        }
    }
    true
}

fn any_eq(vals: &[String], got: Option<&str>) -> bool {
    let Some(g) = got else { return false };
    vals.iter().any(|v| v.eq_ignore_ascii_case(g))
}

fn conversation_matches(c: &Conversation, needle: &str) -> bool {
    let fm = &c.frontmatter;
    if let Some(s) = &fm.name {
        if s.to_lowercase().contains(needle) {
            return true;
        }
    }
    if let Some(s) = &fm.summary {
        if s.to_lowercase().contains(needle) {
            return true;
        }
    }
    c.messages
        .iter()
        .any(|m| m.text.to_lowercase().contains(needle))
}

fn chat_row(c: &Conversation, needle: &str) -> SearchRow {
    let fm = &c.frontmatter;
    let snippet = if !needle.is_empty() {
        first_snippet_in_messages(c, needle).unwrap_or_else(|| {
            fm.summary
                .clone()
                .unwrap_or_else(|| fm.name.clone().unwrap_or_default())
        })
    } else {
        fm.summary
            .clone()
            .unwrap_or_else(|| fm.name.clone().unwrap_or_default())
    };
    SearchRow {
        uuid: fm.uuid.clone(),
        conversation_uuid: fm.uuid.clone(),
        message_index: None,
        snippet,
        sender: String::new(),
        when: fm.updated_at.clone().unwrap_or_default(),
        conversation_name: fm.name.clone().unwrap_or_default(),
        project: fm.project_uuid.clone().unwrap_or_default(),
        account: fm.account_uuid.clone().unwrap_or_default(),
        entire_chat: format!("/chat/{}", fm.uuid),
        source: source_label(&fm.provider),
        kind: "Chat".into(),
        author: String::new(),
    }
}

fn message_row(c: &Conversation, idx: usize, needle: &str) -> SearchRow {
    let fm = &c.frontmatter;
    let m = &c.messages[idx];
    let snippet = if needle.is_empty() {
        first_n_chars(&m.text, SNIPPET_RADIUS * 2)
    } else {
        snippet_around(&m.text, needle)
            .unwrap_or_else(|| first_n_chars(&m.text, SNIPPET_RADIUS * 2))
    };
    let kind = message_kind(&m.sender);
    let author = match kind {
        "User Input" => fm.account_uuid.clone().unwrap_or_else(|| m.sender.clone()),
        "LLM Response" => m.model.clone().unwrap_or_else(|| m.sender.clone()),
        _ => m.model.clone().unwrap_or_else(|| m.sender.clone()),
    };
    SearchRow {
        uuid: format!("{}#m{}", fm.uuid, idx),
        conversation_uuid: fm.uuid.clone(),
        message_index: Some(idx),
        snippet,
        sender: m.sender.clone(),
        when: m.when.clone().unwrap_or_default(),
        conversation_name: fm.name.clone().unwrap_or_default(),
        project: fm.project_uuid.clone().unwrap_or_default(),
        account: fm.account_uuid.clone().unwrap_or_default(),
        entire_chat: format!("/chat/{}", fm.uuid),
        source: source_label(&fm.provider),
        kind: kind.to_string(),
        author,
    }
}

fn first_snippet_in_messages(c: &Conversation, needle: &str) -> Option<String> {
    for m in &c.messages {
        if let Some(s) = snippet_around(&m.text, needle) {
            return Some(s);
        }
    }
    None
}

/// Return ~SNIPPET_RADIUS chars on either side of the first match, char-aligned.
fn snippet_around(haystack: &str, needle: &str) -> Option<String> {
    let lower = haystack.to_lowercase();
    let pos = lower.find(needle)?;
    let start = haystack[..pos]
        .char_indices()
        .rev()
        .nth(SNIPPET_RADIUS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end_byte = pos + needle.len();
    let end = haystack[end_byte..]
        .char_indices()
        .nth(SNIPPET_RADIUS)
        .map(|(i, _)| end_byte + i)
        .unwrap_or(haystack.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(&haystack[start..end]);
    if end < haystack.len() {
        out.push('…');
    }
    Some(out.replace('\n', " "))
}

fn first_n_chars(s: &str, n: usize) -> String {
    let end = s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len());
    let truncated = &s[..end];
    let mut out = truncated.replace('\n', " ");
    if end < s.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qmd::{Conversation, Frontmatter, Message};
    use crate::query::parse_query;
    use std::path::PathBuf;

    fn conv(uuid: &str, name: &str, msgs: &[(&str, &str)]) -> Conversation {
        Conversation {
            path: PathBuf::from(format!("/tmp/{}.qmd", uuid)),
            frontmatter: Frontmatter {
                provider: "anthropic".into(),
                uuid: uuid.into(),
                name: Some(name.into()),
                account_uuid: Some("acct-1".into()),
                project_uuid: Some("proj-1".into()),
                created_at: Some("2025-04-10 14:00:00".into()),
                updated_at: Some("2025-04-10 15:00:00".into()),
                summary: Some(format!("summary of {name}")),
            },
            messages: msgs
                .iter()
                .map(|(sender, text)| Message {
                    sender: (*sender).into(),
                    when: Some("2025-04-10 14:00:00".into()),
                    model: None,
                    text: (*text).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn empty_query_returns_chat_and_messages() {
        let c = vec![conv("a", "Treemap", &[("Human", "hi")])];
        let rows = search(&c, &parse_query(""), 50);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].conversation_uuid, "a");
        assert!(rows[0].message_index.is_none());
        assert_eq!(rows[0].kind, "Chat");
        assert_eq!(rows[1].message_index, Some(0));
        assert_eq!(rows[1].kind, "User Input");
    }

    #[test]
    fn free_text_returns_message_rows_with_snippet() {
        let c = vec![conv(
            "a",
            "Treemap",
            &[("Human", "How do I lay out a squarified treemap?")],
        )];
        let rows = search(&c, &parse_query("squarified"), 50);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_index, Some(0));
        assert!(rows[0].snippet.to_lowercase().contains("squarified"));
        assert_eq!(rows[0].sender, "Human");
    }

    #[test]
    fn type_chat_returns_one_row_per_conversation() {
        let c = vec![conv(
            "a",
            "Treemap",
            &[("Human", "squarified"), ("Assistant", "squarified again")],
        )];
        let rows = search(&c, &parse_query("squarified type:chat"), 50);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].message_index.is_none());
    }

    #[test]
    fn account_filter_drops_others() {
        let mut a = conv("a", "X", &[("Human", "hi")]);
        a.frontmatter.account_uuid = Some("acct-1".into());
        let mut b = conv("b", "Y", &[("Human", "hi")]);
        b.frontmatter.account_uuid = Some("acct-2".into());
        let rows = search(&[a, b], &parse_query("account:acct-1"), 50);
        assert!(rows.iter().all(|r| r.conversation_uuid == "a"));
        assert!(rows.iter().any(|r| r.message_index.is_none()));
    }

    #[test]
    fn before_filter() {
        let mut a = conv("a", "X", &[("Human", "hi")]);
        a.frontmatter.created_at = Some("2024-01-01 00:00:00".into());
        let mut b = conv("b", "Y", &[("Human", "hi")]);
        b.frontmatter.created_at = Some("2026-01-01 00:00:00".into());
        let rows = search(&[a, b], &parse_query("before:2025-01-01"), 50);
        assert!(rows.iter().all(|r| r.conversation_uuid == "a"));
        assert!(rows.iter().any(|r| r.message_index.is_none()));
    }

    #[test]
    fn author_filter_at_message_level() {
        let c = vec![conv(
            "a",
            "X",
            &[
                ("Human", "human says treemap"),
                ("Assistant", "assistant says treemap"),
            ],
        )];
        let rows = search(&c, &parse_query("treemap author:Human"), 50);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "Human");
    }

    #[test]
    fn limit_enforced() {
        let mut convs = Vec::new();
        for i in 0..5 {
            convs.push(conv(&format!("c{i}"), "x", &[("Human", "treemap")]));
        }
        let rows = search(&convs, &parse_query("treemap"), 3);
        assert_eq!(rows.len(), 3);
    }
}
