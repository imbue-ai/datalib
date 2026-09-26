//! Render the SMS/MMS texts and calls into markdown via the shared chat
//! renderer.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use datalib_etl::blob_cas::{BlobBundle, CasEdgeRow};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::RenderProfile;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};
use datalib_etl_chat_common::{render_changed, RenderTarget};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Inputs, RawRange};
use serde_json::Value;

use crate::ids;

use datalib_etl_sms_backup_restore::ingest::schema_raw::SmsAttachmentRow;
use datalib_etl_sms_backup_restore::ingest::{db_path_for, RawDb};
use datalib_schema::providers::Provider;

/// v2: a row whose `date` field is missing or non-numeric gets a null
///     `created_at` instead of a real-looking `1970-01-01T00:00:00`. See
///     `docs/dev/data_architecture_parse_and_render.md` §6.
/// v3: ids are minted through `datalib_id`, every row carries its
///     backpointer, and a message's id carries its stamp in its leading
///     bits (`datalib_id`'s v8 layout). Every uuid moved, `chat_uuid`
///     among them.
pub const RENDER_VERSION: u32 = 3;

/// Projection for [`BlobBundle::load_many`] over the SMS CAS edge: the
/// `ref_name` ({message_id}/{partname}) is the bundle key; `content_type`
/// falls back to `cas_objects`.
const SMS_BLOB_PROJECTION: &str = "SELECT ref_name AS ref_id, blake3, \
            NULL AS content_type, NULL AS upstream_name \
     FROM pinned_sms_attachments sms_attachments \
     WHERE ref_name IN ({placeholders}) AND blake3 IS NOT NULL";

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::SmsBackupRestore,
        // Drives the grid "Source" column (and `source:SMS` queries); keep
        // it short so it reads cleanly next to the SMS icon.
        source_label: "SMS".to_string(),
        chat_kind: "SMS Conversation".to_string(),
        message_kind: "SMS Message".to_string(),
        reaction_kind: "SMS Reaction".to_string(),
        chat_entity_kind: ids::KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

pub use datalib_etl_chat_common::RenderOutcome;

pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    range: RawRange<'_>,
) -> Result<RenderOutcome> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderOutcome::default());
    }
    let (messages, calls, blobs, scan) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Pinned at open. No commit means nothing has been committed
            // here to render, which is emptiness rather than a reason to
            // read the working set.
            let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
                return Ok(Default::default());
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            let loaded = async {
                let messages = datalib_etl::doltlite_raw::load_payloads_with_id(
                    db.pool(),
                    datalib_etl::pin::Reads::At(&pin),
                    "sms_messages",
                )
                .await?;
                let calls = datalib_etl::doltlite_raw::load_payloads_with_id(
                    db.pool(),
                    datalib_etl::pin::Reads::At(&pin),
                    "sms_calls",
                )
                .await?;
                let blobs = load_blobs(&db, &messages).await?;
                let scan = scan_diff(db.pool(), range.cursor, &pin).await?;
                anyhow::Ok((messages, calls, blobs, scan))
            }
            .await;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            loaded
        })
    })?;

    // No early return on an empty store: a reset one still has to
    // name the conversations it lost, so their documents go.
    let all_chats = build_chats(source_id, &messages, &calls);
    render_changed(
        &profile(),
        all_chats,
        scan,
        range,
        |id| ids::conversation(source_id, id).uuid,
        &blobs,
        RenderTarget {
            out_root,
            source_id,
            progress,
            on_doc_complete,
        },
    )
}

/// Which conversations moved since `last_render_hash`.
///
/// The bucket is `"sms:{conversation_key}"` — the same string
/// [`chat_id`] builds, so the changed set compares directly against
/// `NormalizedChat::id`.
///
/// Attachments are not in the union: every chat declares its attachment
/// edges by key, present or not, so a blob filled in by a later run
/// reaches its conversation through the driver's reverse lookup.
async fn scan_diff(
    pool: &sqlx::SqlitePool,
    last_render_hash: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<datalib_etl::doltlite_raw::DiffScan> {
    datalib_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT bucket FROM (
                    SELECT 'sms:' || coalesce(to_conversation_key, from_conversation_key)
                             AS bucket
                      FROM dolt_diff_sms_messages
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT 'sms:' || coalesce(to_conversation_key, from_conversation_key)
                      FROM dolt_diff_sms_calls
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE bucket IS NOT NULL AND bucket != 'sms:'
            ",
        },
    )
    .await
}

async fn load_blobs(
    db: &RawDb,
    messages: &[(String, Value)],
) -> Result<HashMap<String, BlobBundle>> {
    let mut refs_by_chat: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, m) in messages {
        let key = chat_id(m);
        let bag = refs_by_chat.entry(key).or_default();
        for r in attachment_refs(m) {
            bag.push(r);
        }
    }
    BlobBundle::load_many(
        db.pool(),
        db.cas().pool(),
        SMS_BLOB_PROJECTION,
        refs_by_chat,
    )
    .await
}

fn chat_id(v: &Value) -> String {
    let key = v
        .get("conversation_key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("sms:{key}")
}

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

/// Messages and calls as `(row id, payload)`; the id is what the chat
/// declares it read.
fn build_chats(
    source_id: &str,
    messages: &[(String, Value)],
    calls: &[(String, Value)],
) -> Vec<NormalizedChat> {
    let mut by_chat: BTreeMap<String, (Vec<&Value>, Inputs)> = BTreeMap::new();
    for (row_id, v) in messages {
        let (rows, inputs) = by_chat.entry(chat_id(v)).or_default();
        inputs.read("sms_messages", row_id);
        for ref_name in attachment_refs(v) {
            inputs.read(
                "sms_attachments",
                &SmsAttachmentRow::pk_recipe(row_id, &ref_name),
            );
        }
        rows.push(v);
    }
    for (row_id, v) in calls {
        let (rows, inputs) = by_chat.entry(chat_id(v)).or_default();
        inputs.read("sms_calls", row_id);
        rows.push(v);
    }

    let mut chats = Vec::with_capacity(by_chat.len());
    for (id, (rows, inputs)) in by_chat {
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
        let mut items: Vec<NormalizedChatItem> = rows.iter().map(|v| item(source_id, v)).collect();
        items.sort_by_key(|i| i.date_ms);

        let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
        for it in items {
            by_month.entry(month_of(it.date_ms)).or_default().push(it);
        }
        let buckets: Vec<NormalizedDoc> = by_month
            .into_iter()
            .map(|(period_key, items)| {
                let month = ids::month(source_id, &id, &period_key);
                NormalizedDoc {
                    orphan_reactions: Vec::new(),
                    markdown_uuid: month.uuid,
                    source_ref: Some(UpstreamRef::new(month.entity_kind, month.natural_key)),
                    period_key,
                    items,
                }
            })
            .collect();

        let conversation = ids::conversation(source_id, &id);
        chats.push(NormalizedChat {
            inputs: inputs.declared(),
            path_prefix: None,
            id: id.clone(),
            chat_uuid: conversation.uuid,
            display,
            author: None,
            account: None,
            project: Some("SMS Backup".to_string()),
            external_id: Some(conversation.natural_key),
            source_url: None,
            upstream_account: None,
            title: None,
            org_uuid: None,
            org_name: None,
            buckets,
        });
    }
    chats
}

fn item(source_id: &str, v: &Value) -> NormalizedChatItem {
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("sms");
    // Missing or non-numeric `date` is "we don't know when", which is a
    // null `created_at` — not the epoch.
    let date_ms = v.get("date").and_then(Value::as_i64);
    let row_id = v.get("id").and_then(Value::as_str).unwrap_or("");
    let id = if kind == "call" {
        ids::call(source_id, row_id, date_ms)
    } else {
        ids::message(source_id, row_id, date_ms)
    };
    let message_uuid = id.uuid.clone();
    let source_ref = Some(UpstreamRef::new(id.entity_kind, id.natural_key));

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
                source_ref: source_ref.clone(),
                is_aside: false,
                unread: false,
                problems: Vec::new(),
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
                source_ref: source_ref.clone(),
                is_aside: false,
                // Only an explicit `read="0"` on a message someone else
                // sent: an older store's rows carry no `read` at all.
                unread: !is_me && v.get("read").and_then(Value::as_bool) == Some(false),
                problems: Vec::new(),
            }
        }
    }
}

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

fn fmt_duration(s: i64) -> String {
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

fn month_of(ms: Option<i64>) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_millis_opt(ms.unwrap_or(0))
        .single()
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn basename(ref_name: &str) -> &str {
    ref_name
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or(ref_name)
}

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

    fn with_ids(rows: &[Value]) -> Vec<(String, Value)> {
        rows.iter()
            .map(|v| {
                (
                    v.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    v.clone(),
                )
            })
            .collect()
    }

    /// A row with no usable `date` must carry no timestamp — not the
    /// epoch — while still landing in a bucket so it stays reachable.
    #[test]
    fn undated_row_has_no_timestamp_but_still_files() {
        let messages = vec![
            json!({"id":"nd1","kind":"sms","conversation_key":"+1410","conversation_display":"Jean-Luc Picard","is_me":false,"body":"When?","attachments":[]}),
            json!({"id":"nd2","kind":"sms","conversation_key":"+1410","conversation_display":"+1410","date":"not-a-number","is_me":true,"body":"Unclear","attachments":[]}),
        ];
        let chats = build_chats("sms", &with_ids(&messages), &[]);
        let items: Vec<_> = chats[0].buckets.iter().flat_map(|b| &b.items).collect();
        assert_eq!(items.len(), 2, "undated rows are kept, not dropped");
        for i in &items {
            assert_eq!(i.date_ms, None, "{} fabricated a timestamp", i.message_uuid);
        }
        // Filed under the epoch bucket — a filing decision, not a claim
        // about when they happened.
        assert_eq!(chats[0].buckets[0].period_key, "1970-01");
    }

    /// Only a message someone else sent, and the backup says is
    /// unread, renders unread; `read` absent (an older store) is unknown.
    #[test]
    fn only_an_incoming_message_marked_unread_is_unread() {
        let row = |id: &str, is_me: bool, read: Value| {
            json!({"id":id,"kind":"sms","conversation_key":"+1410","conversation_display":"Jean-Luc Picard",
                   "date":1778277198761i64,"is_me":is_me,"body":id,"read":read,"attachments":[]})
        };
        let messages = vec![
            row("unread", false, json!(false)),
            row("read", false, json!(true)),
            row("unknown", false, Value::Null),
            row("mine", true, json!(false)),
        ];
        let chats = build_chats("sms", &with_ids(&messages), &[]);
        let unread: Vec<&str> = chats[0]
            .buckets
            .iter()
            .flat_map(|b| &b.items)
            .filter(|i| i.unread)
            .map(|i| i.text.as_deref().unwrap())
            .collect();
        assert_eq!(unread, vec!["unread"]);
    }

    #[test]
    fn merges_calls_and_texts_into_one_conversation() {
        let messages = vec![
            json!({"id":"m1","kind":"sms","conversation_key":"+1410","conversation_display":"Jean-Luc Picard","date":1778277198761i64,"is_me":false,"body":"Make it so","attachments":[]}),
            json!({"id":"m2","kind":"sms","conversation_key":"+1410","conversation_display":"+1410","date":1778277199000i64,"is_me":true,"body":"Aye","attachments":[]}),
        ];
        let calls = vec![
            json!({"id":"c1","kind":"call","conversation_key":"+1410","conversation_display":"Jean-Luc Picard","date":1778277000000i64,"call_type":"missed","duration":0}),
        ];
        let chats = build_chats("sms", &with_ids(&messages), &with_ids(&calls));
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
            .find(|i| i.source_ref.as_ref().unwrap().native_id == "m2")
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
        let chats = build_chats("sms", &with_ids(&messages), &[]);
        let it = chats[0].buckets[0]
            .items
            .iter()
            .find(|i| i.source_ref.as_ref().unwrap().native_id == "x1")
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
        let chats = build_chats("sms", &[], &with_ids(&calls));
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
