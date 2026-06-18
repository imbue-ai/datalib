//! Render the chat-shaped Takeout feeds into markdown via the shared
//! chat renderer.
//!
//! Two feeds render today: **Google Chat** (`chat_messages`, grouped by
//! their owning `spaces/<id>` prefix) and **Google Voice**
//! (`voice_messages`, grouped by the contact / participant-set the
//! conversation is with, periodized by month). The rest (maps, youtube,
//! gemini, bills) stay queryable in the raw store. Each row maps into a
//! [`NormalizedChatItem`] and the lot is handed to
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

/// Projection for [`BlobBundle::load`] over the Voice CAS edge: the
/// `ref_name` (attachment filename) is the bundle key; `content_type`
/// falls back to `cas_objects` (we don't store it on the edge).
const VOICE_BLOB_PROJECTION: &str = "SELECT ref_name AS ref_id, blake3, \
            NULL AS content_type, NULL AS upstream_name \
     FROM voice_attachments \
     WHERE ref_name IN ({placeholders}) AND blake3 IS NOT NULL";

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

fn voice_profile() -> RenderProfile {
    RenderProfile {
        provider: "google_takeout",
        source_label: "Google Voice".to_string(),
        chat_kind: "Google Voice Conversation".to_string(),
        message_kind: "Google Voice Message".to_string(),
        reaction_kind: "Google Voice Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render the chat-shaped feeds under `raw_dir`. No-op when the raw
/// store is absent; renders whichever of Google Chat / Google Voice has
/// rows.
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
    let (messages, groups, voice_messages, voice_blobs) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open(&db_path).await?;
            let messages = db.load_payloads("chat_messages").await?;
            let groups = db.load_payloads("chat_groups").await?;
            let voice_messages = db.load_payloads("voice_messages").await?;
            let voice_blobs = load_voice_blobs(&db, &voice_messages).await?;
            anyhow::Ok((messages, groups, voice_messages, voice_blobs))
        })
    })?;

    if !messages.is_empty() {
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
    }

    if !voice_messages.is_empty() {
        let chats = build_voice_chats(&voice_messages);
        cc_render_all(
            &voice_profile(),
            &chats,
            out_root,
            source_name,
            &voice_blobs,
            progress,
            prior_fingerprints,
            on_doc_complete,
        )?;
    }
    Ok(())
}

/// Build per-conversation [`BlobBundle`]s for the Voice feed: one bundle
/// per `chat.id`, holding every attachment its messages reference. Keyed
/// to match [`build_voice_chats`]' `chat.id`.
async fn load_voice_blobs(
    db: &RawDb,
    voice_messages: &[Value],
) -> Result<HashMap<String, BlobBundle>> {
    // Group ref_names by conversation_key (== the chat.id we mint).
    let mut refs_by_chat: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in voice_messages {
        let key = voice_chat_id(m);
        let bag = refs_by_chat.entry(key).or_default();
        for r in voice_attachment_refs(m) {
            bag.push(r);
        }
    }
    let mut out: HashMap<String, BlobBundle> = HashMap::new();
    for (chat_id, mut refs) in refs_by_chat {
        refs.sort();
        refs.dedup();
        if refs.is_empty() {
            continue;
        }
        let ref_slices: Vec<&str> = refs.iter().map(String::as_str).collect();
        let bundle = BlobBundle::load(
            db.pool(),
            db.cas().pool(),
            VOICE_BLOB_PROJECTION,
            &ref_slices,
        )
        .await?;
        out.insert(chat_id, bundle);
    }
    Ok(out)
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

// ── Google Voice ────────────────────────────────────────────────────

/// The `NormalizedChat.id` (and `blobs_by_chat` key) for a voice row:
/// its conversation_key, namespaced so it can't collide with a Google
/// Chat space slug.
fn voice_chat_id(m: &Value) -> String {
    let key = m
        .get("conversation_key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("voice:{key}")
}

/// Attachment ref_names referenced by one voice row — the `attachments`
/// array (texts/MMS) plus the single `audio` ref (voicemail / call /
/// recording).
fn voice_attachment_refs(m: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    if let Some(arr) = m.get("attachments").and_then(Value::as_array) {
        refs.extend(arr.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    if let Some(audio) = m.get("audio").and_then(Value::as_str) {
        refs.push(audio.to_string());
    }
    refs
}

/// One [`NormalizedChat`] per conversation (contact or participant set),
/// each periodized into month buckets. Conversations are keyed on the
/// phone number so name-labeled and number-labeled exports of the same
/// contact merge (see `google_voice::derive_channel`).
fn build_voice_chats(messages: &[Value]) -> Vec<NormalizedChat> {
    let mut by_chat: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for m in messages {
        by_chat.entry(voice_chat_id(m)).or_default().push(m);
    }

    let mut chats = Vec::with_capacity(by_chat.len());
    for (chat_id, msgs) in by_chat {
        // Display: prefer a human contact name over a bare number.
        let display = msgs
            .iter()
            .filter_map(|m| m.get("conversation_display").and_then(Value::as_str))
            .find(|d| !d.is_empty() && !d.starts_with('+'))
            .or_else(|| {
                msgs.iter()
                    .filter_map(|m| m.get("conversation_display").and_then(Value::as_str))
                    .find(|d| !d.is_empty())
            })
            .unwrap_or(&chat_id)
            .to_string();
        let external_id = chat_id
            .strip_prefix("voice:")
            .unwrap_or(&chat_id)
            .to_string();

        let mut items: Vec<NormalizedChatItem> = msgs.iter().map(|m| voice_item(m)).collect();
        items.sort_by_key(|i| i.date_ms);

        // Month buckets (`YYYY-MM`), oldest first.
        let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
        for it in items {
            by_month.entry(month_of(it.date_ms)).or_default().push(it);
        }
        let buckets: Vec<NormalizedDoc> = by_month
            .into_iter()
            .map(|(period_key, items)| NormalizedDoc {
                markdown_uuid: uuid5(&format!("voice:doc:{chat_id}:{period_key}")),
                period_key,
                items,
            })
            .collect();

        chats.push(NormalizedChat {
            id: chat_id.clone(),
            chat_uuid: uuid5(&format!("voice:chat:{chat_id}")),
            display,
            account: None,
            project: Some("Google Voice".to_string()),
            external_id: Some(external_id),
            buckets,
        });
    }
    chats
}

/// Map one `voice_messages` payload into a normalized item. Texts/MMS →
/// Text or Attachment; voicemail/recording → Attachment (with the audio
/// blob, transcript as caption); missed/placed/received calls → System.
fn voice_item(m: &Value) -> NormalizedChatItem {
    let kind = m.get("kind").and_then(Value::as_str).unwrap_or("text");
    let message_uuid = m
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| uuid5(&m.to_string()));
    let date_ms = voice_date_ms(m);

    let attachments: Vec<NormalizedAttachment> = voice_attachment_refs(m)
        .into_iter()
        .map(|ref_name| NormalizedAttachment {
            mime_type: voice_mime(&ref_name),
            file_name: Some(ref_name.clone()),
            rel_path: None,
            byte_len: None,
            source_url: None,
            ref_id: Some(ref_name),
        })
        .collect();

    match kind {
        "text" => {
            let is_me = m.get("is_me").and_then(Value::as_bool).unwrap_or(false);
            let sender = m.get("sender");
            let author_display = if is_me {
                "Me".to_string()
            } else {
                sender
                    .and_then(|s| s.get("name").and_then(Value::as_str))
                    .filter(|s| !s.is_empty())
                    .or_else(|| sender.and_then(|s| s.get("tel").and_then(Value::as_str)))
                    .unwrap_or("Unknown")
                    .to_string()
            };
            let author_id = sender
                .and_then(|s| s.get("tel").and_then(Value::as_str))
                .map(str::to_string)
                .unwrap_or_else(|| if is_me { "me".into() } else { "unknown".into() });
            let body = m
                .get("body")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            NormalizedChatItem {
                message_uuid,
                author_id,
                author_display,
                date_ms,
                text: body,
                kind: if attachments.is_empty() {
                    ItemKind::Text
                } else {
                    ItemKind::Attachment
                },
                attachments,
                reactions: Vec::new(),
                system_note: None,
            }
        }
        "voicemail" | "recorded" => {
            let party = m.get("party");
            let author_display = party_display(party);
            let transcript = m.get("transcript").and_then(Value::as_str);
            let label = if kind == "voicemail" {
                "Voicemail"
            } else {
                "Recorded call"
            };
            let caption = match transcript {
                Some(t) if !t.is_empty() => format!("**{label}:** {t}"),
                _ => format!("**{label}**"),
            };
            NormalizedChatItem {
                message_uuid,
                author_id: party_id(party),
                author_display,
                date_ms,
                text: Some(caption),
                // Attachment so the audio blob renders an inline player.
                kind: ItemKind::Attachment,
                attachments,
                reactions: Vec::new(),
                system_note: None,
            }
        }
        // missed / placed / received — a call with no media: a system note.
        other => {
            let party = m.get("party");
            let note = match other {
                "missed" => "Missed call",
                "placed" => "Placed call",
                "received" => "Received call",
                _ => "Call",
            };
            NormalizedChatItem {
                message_uuid,
                author_id: party_id(party),
                author_display: party_display(party),
                date_ms,
                text: None,
                kind: ItemKind::System,
                attachments: Vec::new(),
                reactions: Vec::new(),
                system_note: Some(format!("{note} — {}", party_display(party))),
            }
        }
    }
}

fn party_display(party: Option<&Value>) -> String {
    party
        .and_then(|p| p.get("name").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .or_else(|| party.and_then(|p| p.get("tel").and_then(Value::as_str)))
        .unwrap_or("Unknown")
        .to_string()
}

fn party_id(party: Option<&Value>) -> String {
    party
        .and_then(|p| p.get("tel").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".to_string())
}

/// Unix millis from the canonical `when` (RFC3339), falling back to the
/// raw value then 0.
fn voice_date_ms(m: &Value) -> i64 {
    let ts = m
        .get("when")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| m.get("when_raw").and_then(Value::as_str))
        .unwrap_or("");
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
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

/// MIME guess from an attachment filename's extension, so chat-common
/// can pick `<img>` / `<audio>` / `<video>` / link rendering.
fn voice_mime(name: &str) -> Option<String> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "amr" => "audio/amr",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "3gp" | "3gpp" => "video/3gpp",
        "mp4" => "video/mp4",
        _ => return None,
    };
    Some(ct.to_string())
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

    #[test]
    fn voice_groups_by_contact_and_buckets_by_month() {
        let messages = vec![
            json!({"id":"u1","kind":"text","conversation_key":"+1410","conversation_display":"Wes Blackwell","when":"2019-08-01T14:49:00.742-07:00","sender":{"tel":"+1410","name":"Wes Blackwell"},"is_me":false,"body":"Hello","attachments":[]}),
            json!({"id":"u2","kind":"text","conversation_key":"+1410","conversation_display":"+1410","when":"2019-09-02T10:00:00.000-07:00","sender":{"tel":"+6506","name":null},"is_me":true,"body":"Hi back","attachments":[]}),
        ];
        let chats = build_voice_chats(&messages);
        assert_eq!(chats.len(), 1);
        // Human name wins over the bare-number display.
        assert_eq!(chats[0].display, "Wes Blackwell");
        assert_eq!(chats[0].id, "voice:+1410");
        // Two distinct months → two buckets.
        assert_eq!(chats[0].buckets.len(), 2);
        assert_eq!(chats[0].buckets[0].period_key, "2019-08");
        assert_eq!(chats[0].buckets[1].period_key, "2019-09");
        // is_me → "Me".
        assert_eq!(chats[0].buckets[1].items[0].author_display, "Me");
    }

    #[test]
    fn voicemail_is_attachment_with_transcript_caption() {
        let messages = vec![json!({
            "id":"v1","kind":"voicemail","conversation_key":"+1555","conversation_display":"Jean-Luc Picard",
            "when":"2010-02-18T16:10:05.000-08:00","party":{"tel":"+1555","name":"Jean-Luc Picard"},
            "transcript":"Make it so.","duration":"PT13S","audio":"vm.mp3"
        })];
        let chats = build_voice_chats(&messages);
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(item.text.as_deref(), Some("**Voicemail:** Make it so."));
        assert_eq!(item.attachments.len(), 1);
        assert_eq!(item.attachments[0].ref_id.as_deref(), Some("vm.mp3"));
        assert_eq!(item.attachments[0].mime_type.as_deref(), Some("audio/mpeg"));
    }

    #[test]
    fn missed_call_is_system_note() {
        let messages = vec![json!({
            "id":"c1","kind":"missed","conversation_key":"+1999","conversation_display":"Spammer",
            "when":"2009-03-06T09:50:34.000-08:00","party":{"tel":"+1999","name":"Spammer"}
        })];
        let chats = build_voice_chats(&messages);
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::System);
        assert_eq!(item.system_note.as_deref(), Some("Missed call — Spammer"));
    }

    #[test]
    fn voice_mms_text_becomes_attachment_item() {
        let messages = vec![json!({
            "id":"t1","kind":"text","conversation_key":"+1202","conversation_display":"+1202",
            "when":"2024-02-02T09:06:01.024-08:00","sender":{"tel":"+1202","name":null},"is_me":false,
            "body":"pic","attachments":["+1202 - Text - x-1-1.jpg"]
        })];
        let chats = build_voice_chats(&messages);
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(item.attachments[0].mime_type.as_deref(), Some("image/jpeg"));
    }
}
