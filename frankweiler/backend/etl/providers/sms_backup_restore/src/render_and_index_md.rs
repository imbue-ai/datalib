//! Render the SMS/MMS texts and calls into markdown via the shared chat
//! renderer.
//!
//! One [`NormalizedChat`] per phone number, periodized by month. Texts
//! (`<sms>`) render as messages; MMS render as messages carrying their
//! image/audio attachments; calls fold into the same conversation as
//! inline system notes — mirroring how Google Voice merges calls and
//! texts for a contact. Each row maps into a [`NormalizedChatItem`] and
//! the lot is handed to
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
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use serde_json::Value;
use uuid::Uuid;

use crate::extract::{db_path_for, RawDb};

const RENDER_VERSION: u32 = 1;

/// Projection for [`BlobBundle::load`] over the SMS CAS edge: the
/// `ref_name` ({message_id}/{partname}) is the bundle key; `content_type`
/// falls back to `cas_objects`.
const SMS_BLOB_PROJECTION: &str = "SELECT ref_name AS ref_id, blake3, \
            NULL AS content_type, NULL AS upstream_name \
     FROM sms_attachments \
     WHERE ref_name IN ({placeholders}) AND blake3 IS NOT NULL";

fn ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"sms-backup-restore-chat.frankweiler")
}

fn uuid5(recipe: &str) -> String {
    Uuid::new_v5(&ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "sms_backup_restore",
        // Drives the grid "Source" column (and `source:SMS` queries); keep
        // it short so it reads cleanly next to the SMS icon.
        source_label: "SMS".to_string(),
        chat_kind: "SMS Conversation".to_string(),
        message_kind: "SMS Message".to_string(),
        reaction_kind: "SMS Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render the texts + calls under `raw_dir`. No-op when the raw store is
/// absent or empty.
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
    let (messages, calls, blobs) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open(&db_path).await?;
            let messages = db.load_payloads("sms_messages").await?;
            let calls = db.load_payloads("sms_calls").await?;
            let blobs = load_blobs(&db, &messages).await?;
            anyhow::Ok((messages, calls, blobs))
        })
    })?;

    if messages.is_empty() && calls.is_empty() {
        return Ok(());
    }
    let chats = build_chats(&messages, &calls);
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

/// One [`BlobBundle`] per conversation (`chat.id`), holding every
/// attachment its MMS reference. Keyed to match [`build_chats`]' `chat.id`.
async fn load_blobs(db: &RawDb, messages: &[Value]) -> Result<HashMap<String, BlobBundle>> {
    let mut refs_by_chat: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in messages {
        let key = chat_id(m);
        let bag = refs_by_chat.entry(key).or_default();
        for r in attachment_refs(m) {
            bag.push(r);
        }
    }
    let mut out: HashMap<String, BlobBundle> = HashMap::new();
    for (id, mut refs) in refs_by_chat {
        refs.sort();
        refs.dedup();
        if refs.is_empty() {
            continue;
        }
        let slices: Vec<&str> = refs.iter().map(String::as_str).collect();
        let bundle =
            BlobBundle::load(db.pool(), db.cas().pool(), SMS_BLOB_PROJECTION, &slices).await?;
        out.insert(id, bundle);
    }
    Ok(out)
}

/// The `NormalizedChat.id` (and `blobs_by_chat` key) for a row: its
/// conversation_key, namespaced.
fn chat_id(v: &Value) -> String {
    let key = v
        .get("conversation_key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("sms:{key}")
}

/// Attachment ref names referenced by one message row.
fn attachment_refs(v: &Value) -> Vec<String> {
    v.get("attachments")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// One [`NormalizedChat`] per conversation, month-bucketed, merging
/// texts and calls keyed on the same phone number.
fn build_chats(messages: &[Value], calls: &[Value]) -> Vec<NormalizedChat> {
    let mut by_chat: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for v in messages.iter().chain(calls.iter()) {
        by_chat.entry(chat_id(v)).or_default().push(v);
    }

    let mut chats = Vec::with_capacity(by_chat.len());
    for (id, rows) in by_chat {
        // Display: prefer a human contact name over a bare number.
        let display = rows
            .iter()
            .filter_map(|m| m.get("conversation_display").and_then(Value::as_str))
            .find(|d| {
                !d.is_empty()
                    && !d
                        .chars()
                        .next()
                        .is_some_and(|c| c == '+' || c.is_ascii_digit())
            })
            .or_else(|| {
                rows.iter()
                    .filter_map(|m| m.get("conversation_display").and_then(Value::as_str))
                    .find(|d| !d.is_empty())
            })
            .unwrap_or(&id)
            .to_string();
        let external_id = id.strip_prefix("sms:").unwrap_or(&id).to_string();

        let mut items: Vec<NormalizedChatItem> = rows.iter().map(|v| item(v)).collect();
        items.sort_by_key(|i| i.date_ms);

        let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
        for it in items {
            by_month.entry(month_of(it.date_ms)).or_default().push(it);
        }
        let buckets: Vec<NormalizedDoc> = by_month
            .into_iter()
            .map(|(period_key, items)| NormalizedDoc {
                markdown_uuid: uuid5(&format!("doc:{id}:{period_key}")),
                period_key,
                items,
            })
            .collect();

        chats.push(NormalizedChat {
            id: id.clone(),
            chat_uuid: uuid5(&format!("chat:{id}")),
            display,
            account: None,
            project: Some("SMS Backup".to_string()),
            external_id: Some(external_id),
            source_url: None,
            title: None,
            org_uuid: None,
            org_name: None,
            buckets,
        });
    }
    chats
}

/// Map one row (sms/mms message or call) into a normalized item.
fn item(v: &Value) -> NormalizedChatItem {
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("sms");
    let message_uuid = v
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid5(&v.to_string()));
    let date_ms = v.get("date").and_then(Value::as_i64).unwrap_or(0);

    match kind {
        "call" => {
            let display = v
                .get("conversation_display")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            let call_type = v.get("call_type").and_then(Value::as_str).unwrap_or("call");
            let duration = v.get("duration").and_then(Value::as_i64).unwrap_or(0);
            NormalizedChatItem {
                message_uuid,
                author_id: v
                    .get("conversation_key")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                author_display: display.to_string(),
                date_ms,
                text: None,
                kind: ItemKind::System,
                attachments: Vec::new(),
                reactions: Vec::new(),
                system_note: Some(call_note(call_type, duration, display)),
                source_url: None,
                kind_label: None,
            }
        }
        // sms / mms
        _ => {
            let is_me = v.get("is_me").and_then(Value::as_bool).unwrap_or(false);
            let display = v
                .get("conversation_display")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            let author_display = if is_me {
                "Me".to_string()
            } else {
                display.to_string()
            };
            let author_id = if is_me {
                "me".to_string()
            } else {
                v.get("conversation_key")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string()
            };
            // SMS body lives in `body`; MMS body in `text`.
            let text = v
                .get("body")
                .and_then(Value::as_str)
                .or_else(|| v.get("text").and_then(Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string);

            let attachments: Vec<NormalizedAttachment> = attachment_refs(v)
                .into_iter()
                .map(|ref_name| {
                    let base = basename(&ref_name);
                    NormalizedAttachment {
                        mime_type: mime_for(base),
                        file_name: Some(base.to_string()),
                        rel_path: None,
                        byte_len: None,
                        source_url: None,
                        ref_id: Some(ref_name),
                    }
                })
                .collect();

            NormalizedChatItem {
                message_uuid,
                author_id,
                author_display,
                date_ms,
                text,
                kind: if attachments.is_empty() {
                    ItemKind::Text
                } else {
                    ItemKind::Attachment
                },
                attachments,
                reactions: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: None,
            }
        }
    }
}

/// A human-readable system note for a call.
fn call_note(call_type: &str, duration_s: i64, display: &str) -> String {
    let label = match call_type {
        "incoming" => "Incoming call",
        "outgoing" => "Outgoing call",
        "missed" => "Missed call",
        "voicemail" => "Voicemail",
        "rejected" => "Rejected call",
        "blocked" => "Blocked call",
        _ => "Call",
    };
    if duration_s > 0 {
        format!("{label} — {display} ({})", fmt_duration(duration_s))
    } else {
        format!("{label} — {display}")
    }
}

/// `m:ss` (or `h:mm:ss`) for a call duration in seconds.
fn fmt_duration(s: i64) -> String {
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// `YYYY-MM` (UTC) bucket key for a unix-millis timestamp.
fn month_of(ms: i64) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Basename of a `{message_id}/{partname}` ref.
fn basename(ref_name: &str) -> &str {
    ref_name
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or(ref_name)
}

/// MIME guess from an attachment filename's extension, so chat-common
/// can pick `<img>` / `<audio>` / `<video>` / link rendering.
fn mime_for(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "amr" => "audio/amr",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/x-wav",
        "3gp" | "3gpp" => "video/3gpp",
        "mp4" => "video/mp4",
        "vcf" => "text/vcard",
        _ => return None,
    };
    Some(ct.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merges_calls_and_texts_into_one_conversation() {
        let messages = vec![
            json!({"id":"m1","kind":"sms","conversation_key":"+1410","conversation_display":"Jean-Luc Picard","date":1778277198761i64,"is_me":false,"body":"Make it so","attachments":[]}),
            json!({"id":"m2","kind":"sms","conversation_key":"+1410","conversation_display":"+1410","date":1778277199000i64,"is_me":true,"body":"Aye","attachments":[]}),
        ];
        let calls = vec![
            json!({"id":"c1","kind":"call","conversation_key":"+1410","conversation_display":"Jean-Luc Picard","date":1778277000000i64,"call_type":"missed","duration":0}),
        ];
        let chats = build_chats(&messages, &calls);
        assert_eq!(chats.len(), 1, "calls + texts on one number → one chat");
        assert_eq!(chats[0].display, "Jean-Luc Picard");
        // 3 items across the buckets (2 texts + 1 call).
        let n: usize = chats[0].buckets.iter().map(|b| b.items.len()).sum();
        assert_eq!(n, 3);
        // The sent text shows as "Me".
        let me = chats[0]
            .buckets
            .iter()
            .flat_map(|b| &b.items)
            .find(|i| i.message_uuid == "m2")
            .unwrap();
        assert_eq!(me.author_display, "Me");
    }

    #[test]
    fn mms_with_image_is_attachment_item() {
        let messages = vec![json!({
            "id":"x1","kind":"mms","conversation_key":"+1555","conversation_display":"+1555",
            "date":1781811656000i64,"is_me":false,"text":"Happy Thurs",
            "attachments":["x1/image000001.gif"]
        })];
        let chats = build_chats(&messages, &[]);
        let it = chats[0].buckets[0]
            .items
            .iter()
            .find(|i| i.message_uuid == "x1")
            .unwrap();
        assert_eq!(it.kind, ItemKind::Attachment);
        assert_eq!(it.text.as_deref(), Some("Happy Thurs"));
        assert_eq!(it.attachments.len(), 1);
        assert_eq!(
            it.attachments[0].file_name.as_deref(),
            Some("image000001.gif")
        );
        assert_eq!(it.attachments[0].mime_type.as_deref(), Some("image/gif"));
        assert_eq!(
            it.attachments[0].ref_id.as_deref(),
            Some("x1/image000001.gif")
        );
    }

    #[test]
    fn missed_call_is_system_note() {
        let calls = vec![json!({
            "id":"c9","kind":"call","conversation_key":"+1999","conversation_display":"Q",
            "date":1778277000000i64,"call_type":"missed","duration":0
        })];
        let chats = build_chats(&[], &calls);
        let it = &chats[0].buckets[0].items[0];
        assert_eq!(it.kind, ItemKind::System);
        assert_eq!(it.system_note.as_deref(), Some("Missed call — Q"));
    }

    #[test]
    fn outgoing_call_note_has_duration() {
        assert_eq!(fmt_duration(42), "0:42");
        assert_eq!(fmt_duration(125), "2:05");
        assert_eq!(fmt_duration(3661), "1:01:01");
        assert_eq!(
            call_note("outgoing", 42, "Worf"),
            "Outgoing call — Worf (0:42)"
        );
    }
}
