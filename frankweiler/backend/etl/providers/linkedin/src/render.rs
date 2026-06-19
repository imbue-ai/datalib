//! Render LinkedIn's message-shaped feeds into markdown via the shared
//! chat renderer.
//!
//! Conversations are the only LinkedIn feeds we render; every other CSV
//! lands in the raw store for query and stops there. Several files share
//! the `messages.csv` schema (`CONVERSATION ID, FROM, TO, DATE, CONTENT,
//! …`): the primary `messages` direct-message feed plus the AI-coach
//! transcripts (`guide_messages`, `learning_coach_messages`, …). We
//! render every one of them — see [`schema_raw::message_tables`]. For
//! each present, non-empty table we group rows by `CONVERSATION ID`, map
//! each into a [`NormalizedChatItem`], and hand the lot to
//! [`frankweiler_etl_chat_common::render::render_all`], which owns all
//! the markdown / grid-row / fingerprint plumbing. Chat/message ids are
//! namespaced by table so feeds can't collide on a shared conversation
//! id.

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

use crate::extract::schema_raw::{message_tables, ns_id as uuid5};
use crate::extract::{db_path_for, RawDb};

/// Bump when the item-shape / column mapping changes meaningfully.
const RENDER_VERSION: u32 = 1;

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

/// Render every message-shaped table under `raw_dir` into `out_dir`.
/// No-op if the raw store is absent; each individual table is skipped if
/// it's missing or empty. Conversations from different feeds keep
/// distinct ids (namespaced by table) so they never collide.
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

    let mut chats: Vec<NormalizedChat> = Vec::new();
    for table in message_tables() {
        let payloads = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let db = RawDb::open(&db_path).await?;
                // A feed the user didn't export has no table; treat a
                // load error as "absent" rather than failing the render.
                Ok::<_, anyhow::Error>(db.load_payloads(table).await.unwrap_or_default())
            })
        })?;
        chats.extend(build_chats(table, &payloads));
    }

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

/// One [`NormalizedChat`] per `CONVERSATION ID` in a single message
/// table, single `all` bucket, items sorted oldest-first. Ids are
/// namespaced by `table` so two feeds can't clash on a conversation id.
fn build_chats(table: &str, payloads: &[Value]) -> Vec<NormalizedChat> {
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
                    message_uuid: uuid5(&format!("msg:{table}:{conv}:{date}:{from}:{content}")),
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
                    source_url: None,
                }
            })
            .collect();
        items.sort_by_key(|i| i.date_ms);

        let display = nonempty(field(rows[0], "CONVERSATION TITLE"))
            .map(str::to_string)
            .unwrap_or_else(|| participants(&rows));

        chats.push(NormalizedChat {
            id: format!("{table}:{conv}"),
            chat_uuid: uuid5(&format!("chat:{table}:{conv}")),
            display,
            title: None,
            account: None,
            project: None,
            external_id: Some(conv.clone()),
            // No public per-conversation URL in the message export.
            source_url: None,
            buckets: vec![NormalizedDoc {
                period_key: "all".to_string(),
                markdown_uuid: uuid5(&format!("doc:{table}:{conv}:all")),
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
        let chats = build_chats("messages", &payloads);
        assert_eq!(chats.len(), 2);
        let c1 = chats.iter().find(|c| c.id == "messages:c1").unwrap();
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
