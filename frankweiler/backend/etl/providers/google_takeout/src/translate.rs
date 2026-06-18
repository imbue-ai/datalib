//! Render the Google Chat feed into markdown via the shared chat
//! renderer.
//!
//! Google Chat is the only Takeout feed we render today; the rest
//! (maps, youtube, gemini) stay queryable in the raw store. We group the
//! `chat_messages` table by their owning space — derived from the
//! `spaces/<id>/…` prefix of each `message_id`, so we need only the
//! payloads, not the promoted `group_id` column — map each row into a
//! [`NormalizedChatItem`], and hand the lot to
//! [`frankweiler_etl_chat_common::render::render_all`].

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

const RENDER_VERSION: u32 = 1;

fn ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"google-takeout-chat.frankweiler")
}

fn uuid5(recipe: &str) -> String {
    Uuid::new_v5(&ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "google_takeout",
        source_label: "Google Chat".to_string(),
        chat_kind: "Google Chat".to_string(),
        message_kind: "Google Chat Message".to_string(),
        reaction_kind: "Google Chat Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render the `chat_messages` table under `raw_dir`. No-op when the raw
/// store (or the chat feed) is absent / empty.
pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(());
    }
    let (messages, groups) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open(&db_path).await?;
            let messages = db.load_payloads("chat_messages").await?;
            let groups = db.load_payloads("chat_groups").await?;
            anyhow::Ok((messages, groups))
        })
    })?;
    if messages.is_empty() {
        return Ok(());
    }

    let chats = build_chats(&messages, &groups);
    let blobs: HashMap<String, BlobBundle> = HashMap::new();
    cc_render_all(
        &profile(),
        &chats,
        out_root,
        source_name,
        &blobs,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )?;
    Ok(())
}

/// One [`NormalizedChat`] per space, items sorted oldest-first.
fn build_chats(messages: &[Value], groups: &[Value]) -> Vec<NormalizedChat> {
    // space_name -> participant display, from group_info payloads.
    let mut display_by_space: HashMap<String, String> = HashMap::new();
    for g in groups {
        if let Some(space) = g.get("name").and_then(Value::as_str) {
            let members: Vec<&str> = g
                .get("members")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("name").and_then(Value::as_str))
                        .collect()
                })
                .unwrap_or_default();
            if !members.is_empty() {
                display_by_space.insert(space.to_string(), members.join(", "));
            }
        }
    }

    let mut by_space: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for m in messages {
        let space = space_of(m.get("message_id").and_then(Value::as_str).unwrap_or(""));
        by_space.entry(space).or_default().push(m);
    }

    let mut chats = Vec::with_capacity(by_space.len());
    for (space, msgs) in by_space {
        let mut items: Vec<NormalizedChatItem> = msgs
            .iter()
            .map(|m| {
                let creator = m.get("creator");
                let name = creator
                    .and_then(|c| c.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown");
                let email = creator
                    .and_then(|c| c.get("email"))
                    .and_then(Value::as_str)
                    .unwrap_or(name);
                let id = m.get("message_id").and_then(Value::as_str).unwrap_or("");
                let text = m.get("text").and_then(Value::as_str);
                NormalizedChatItem {
                    message_uuid: uuid5(&format!("msg:{id}")),
                    author_id: email.to_string(),
                    author_display: name.to_string(),
                    date_ms: parse_date_ms(
                        m.get("created_date").and_then(Value::as_str).unwrap_or(""),
                    ),
                    text: text.filter(|s| !s.is_empty()).map(str::to_string),
                    kind: ItemKind::Text,
                    attachments: Vec::new(),
                    reactions: Vec::new(),
                    system_note: None,
                }
            })
            .collect();
        items.sort_by_key(|i| i.date_ms);

        let display = display_by_space
            .get(&space)
            .cloned()
            .unwrap_or_else(|| space.clone());

        chats.push(NormalizedChat {
            id: space.clone(),
            chat_uuid: uuid5(&format!("chat:{space}")),
            display,
            account: None,
            project: None,
            external_id: Some(space.clone()),
            buckets: vec![NormalizedDoc {
                period_key: "all".to_string(),
                markdown_uuid: uuid5(&format!("doc:{space}:all")),
                items,
            }],
        });
    }
    chats
}

/// `spaces/AAAA/topics/T1/messages/M1` -> `spaces/AAAA`. Falls back to
/// the whole id when it doesn't have the expected shape.
fn space_of(message_id: &str) -> String {
    let parts: Vec<&str> = message_id.split('/').collect();
    if parts.len() >= 2 && parts[0] == "spaces" {
        format!("spaces/{}", parts[1])
    } else {
        message_id.to_string()
    }
}

/// Parse Google Chat's `Tuesday, February 11, 2025 at 11:33:35 AM UTC`
/// timestamp to unix millis. Returns 0 on any unexpected shape.
fn parse_date_ms(s: &str) -> i64 {
    let s = s.trim();
    chrono::NaiveDateTime::parse_from_str(s, "%A, %B %d, %Y at %I:%M:%S %p UTC")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%A, %B %e, %Y at %I:%M:%S %p UTC"))
        .map(|dt| dt.and_utc().timestamp_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_by_space_and_renders_display() {
        let messages = vec![
            json!({"message_id":"spaces/AAA/topics/T1/messages/M2","created_date":"Tuesday, February 11, 2025 at 11:34:00 AM UTC","creator":{"name":"William Riker","email":"r@e"},"text":"Aye, sir."}),
            json!({"message_id":"spaces/AAA/topics/T1/messages/M1","created_date":"Tuesday, February 11, 2025 at 11:33:35 AM UTC","creator":{"name":"Jean-Luc Picard","email":"p@e"},"text":"Set a course."}),
        ];
        let groups = vec![
            json!({"name":"spaces/AAA","members":[{"name":"Jean-Luc Picard"},{"name":"William Riker"}]}),
        ];
        let chats = build_chats(&messages, &groups);
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].display, "Jean-Luc Picard, William Riker");
        assert_eq!(
            chats[0].buckets[0].items[0].text.as_deref(),
            Some("Set a course.")
        );
    }

    #[test]
    fn space_prefix() {
        assert_eq!(space_of("spaces/AAA/topics/T1/messages/M1"), "spaces/AAA");
        assert_eq!(space_of("weird"), "weird");
    }
}
