//! Render LinkedIn `messages` into markdown via the shared chat
//! renderer.
//!
//! Messages are the only LinkedIn feed we render today; every other CSV
//! lands in the raw store for query and stops there. We group the
//! `messages` table by `CONVERSATION ID`, map each row into a
//! [`NormalizedChatItem`], and hand the lot to
//! [`frankweiler_etl_chat_common::render::render_all`], which owns all
//! the markdown / grid-row / fingerprint plumbing.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use serde_json::Value;
use uuid::Uuid;

use crate::extract::{db_path_for, RawDb};

/// Bump when the item-shape / column mapping changes meaningfully.
const RENDER_VERSION: u32 = 1;

fn linkedin_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"linkedin.frankweiler")
}

fn uuid5(recipe: &str) -> String {
    Uuid::new_v5(&linkedin_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "linkedin",
        source_label: "LinkedIn".to_string(),
        chat_kind: "LinkedIn Chat".to_string(),
        message_kind: "LinkedIn Message".to_string(),
        reaction_kind: "LinkedIn Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render the `messages` table under `raw_dir` into `out_dir`. No-op if
/// the raw store (or the messages table) is absent / empty.
pub fn render(
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(());
    }
    let payloads = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async { RawDb::open(&db_path).await?.load_payloads("messages").await })
    })?;

    let chats = build_chats(&payloads);
    let blobs: HashMap<String, BlobBundle> = HashMap::new();
    cc_render_all(
        &profile(),
        &chats,
        out_dir,
        source_name,
        &blobs,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )?;
    Ok(())
}

/// One [`NormalizedChat`] per `CONVERSATION ID`, single `all` bucket,
/// items sorted oldest-first.
fn build_chats(payloads: &[Value]) -> Vec<NormalizedChat> {
    // BTreeMap keeps conversation order stable across runs.
    let mut by_conv: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for p in payloads {
        let conv = field(p, "CONVERSATION ID");
        by_conv.entry(conv.to_string()).or_default().push(p);
    }

    let mut chats = Vec::with_capacity(by_conv.len());
    for (conv, rows) in by_conv {
        let mut items: Vec<NormalizedChatItem> = rows
            .iter()
            .map(|p| {
                let from = field(p, "FROM");
                let date = field(p, "DATE");
                let content = field(p, "CONTENT");
                NormalizedChatItem {
                    message_uuid: uuid5(&format!("msg:{conv}:{date}:{from}:{content}")),
                    author_id: nonempty(field(p, "SENDER PROFILE URL"))
                        .unwrap_or(from)
                        .to_string(),
                    author_display: nonempty(from).unwrap_or("Unknown").to_string(),
                    date_ms: parse_date_ms(date),
                    text: nonempty(content).map(str::to_string),
                    kind: ItemKind::Text,
                    attachments: Vec::new(),
                    reactions: Vec::new(),
                    system_note: None,
                }
            })
            .collect();
        items.sort_by_key(|i| i.date_ms);

        let display = nonempty(field(rows[0], "CONVERSATION TITLE"))
            .map(str::to_string)
            .unwrap_or_else(|| participants(&rows));

        chats.push(NormalizedChat {
            id: conv.clone(),
            chat_uuid: uuid5(&format!("chat:{conv}")),
            display,
            account: None,
            project: None,
            external_id: Some(conv.clone()),
            buckets: vec![NormalizedDoc {
                period_key: "all".to_string(),
                markdown_uuid: uuid5(&format!("doc:{conv}:all")),
                items,
            }],
        });
    }
    chats
}

/// Distinct participant names across a conversation, in first-seen
/// order, joined for the page title when LinkedIn gave no explicit one.
fn participants(rows: &[&Value]) -> String {
    let mut seen = Vec::new();
    for p in rows {
        for key in ["FROM", "TO"] {
            if let Some(name) = nonempty(field(p, key)) {
                if !seen.iter().any(|n| n == name) {
                    seen.push(name.to_string());
                }
            }
        }
    }
    if seen.is_empty() {
        "LinkedIn conversation".to_string()
    } else {
        seen.join(", ")
    }
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("")
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// Parse LinkedIn's `2026-06-16 22:11:33 UTC` timestamp to unix millis.
/// Returns 0 on any unexpected shape (sorts such rows to the top).
fn parse_date_ms(s: &str) -> i64 {
    let s = s.trim().trim_end_matches(" UTC").trim();
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(conv: &str, from: &str, to: &str, date: &str, content: &str) -> Value {
        json!({
            "CONVERSATION ID": conv, "CONVERSATION TITLE": "",
            "FROM": from, "TO": to, "DATE": date, "CONTENT": content,
            "SENDER PROFILE URL": "",
        })
    }

    #[test]
    fn groups_by_conversation_and_sorts() {
        let payloads = vec![
            msg("c1", "A", "B", "2026-06-16 22:11:33 UTC", "second"),
            msg("c1", "B", "A", "2026-06-16 04:58:21 UTC", "first"),
            msg("c2", "A", "C", "2026-01-01 00:00:00 UTC", "other"),
        ];
        let chats = build_chats(&payloads);
        assert_eq!(chats.len(), 2);
        let c1 = chats.iter().find(|c| c.id == "c1").unwrap();
        assert_eq!(c1.buckets[0].items.len(), 2);
        assert_eq!(c1.buckets[0].items[0].text.as_deref(), Some("first"));
        assert_eq!(c1.display, "A, B");
    }

    #[test]
    fn parses_timestamp() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 6, 16)
            .unwrap()
            .and_hms_opt(22, 11, 33)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(parse_date_ms("2026-06-16 22:11:33 UTC"), expected);
        assert_eq!(parse_date_ms(""), 0);
    }
}
