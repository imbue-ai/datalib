//! Render the SMS/MMS texts and calls into markdown via the shared chat
//! renderer.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use datalib_etl::blob_cas::CasEdgeRow;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{RenderProfile, TextFormat};
use datalib_etl_chat_common::types::{
    AuthorKind, ItemKind, NamedPerson, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, Recipient, RecipientRole, UpstreamRef,
};
use datalib_etl_chat_common::{render_changed_batched, ChatBlobs, RenderTarget};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Inputs, RawRange};
use serde_json::Value;

use crate::ids;

use datalib_etl_sms_backup_restore::ingest::schema_raw::SmsAttachmentRow;
use datalib_etl_sms_backup_restore::ingest::{db_path_for, RawDb};
use datalib_handle::Handle;
use datalib_schema::providers::Provider;

/// v2: a row whose `date` field is missing or non-numeric gets a null
///     `created_at` instead of a real-looking `1970-01-01T00:00:00`. See
///     `docs/dev/data_architecture_parse_and_render.md` §6.
/// v3: ids are minted through `datalib_id`, every row carries its
///     backpointer, and a message's id carries its stamp in its leading
///     bits (`datalib_id`'s v8 layout). Every uuid moved, `chat_uuid`
///     among them.
/// v5: an incoming message's author carries the sender's `tel:` handle.
/// v6: a `+1` number without ten digits after the 1 has no handle.
/// v7: a call carries the other side's `tel:` handle, and a text the
///     account sent names whom it went to, so both reach `with:`.
pub const RENDER_VERSION: u32 = 7;

/// Projection for `BlobBundle::load_many` over the SMS CAS edge: the
/// `ref_name` ({message_id}/{partname}) is the bundle key; `content_type`
/// falls back to `cas_objects`.
const SMS_BLOB_PROJECTION: &str = "SELECT ref_name AS ref_id, blake3, \
            NULL AS content_type, NULL AS upstream_name \
     FROM sms_attachments \
     WHERE ref_name IN ({placeholders}) AND blake3 IS NOT NULL";

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::SmsBackupRestore,
        // Drives the grid "Source" column; keep
        // it short so it reads cleanly next to the SMS icon.
        source_label: "SMS".to_string(),
        chat_kind: "SMS Conversation".to_string(),
        message_kind: "SMS Message".to_string(),
        reaction_kind: "SMS Reaction".to_string(),
        chat_entity_kind: ids::KIND_CONVERSATION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Plain,
    }
}

pub use datalib_etl_chat_common::RenderOutcome;

/// Renders the store under `raw_dir`, its attachment bytes read a batch
/// of at most `blob_bytes` at a time.
pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    range: RawRange<'_>,
    blob_bytes: u64,
) -> Result<RenderOutcome> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderOutcome::default());
    }
    // Pinned at open. No commit means nothing has been committed here to
    // render, which is emptiness rather than a reason to read the working
    // set.
    let Some(db) = block_on(RawDb::open_reader(&db_path, range.pin))? else {
        return Ok(RenderOutcome::default());
    };
    let rendered = render_open(
        &db,
        source_id,
        range,
        blob_bytes,
        RenderTarget {
            out_root,
            source_id,
            progress,
            on_doc_complete,
        },
    );
    // Closed, not dropped: the next open of this store is a second
    // connection until this one is actually gone.
    block_on(db.close());
    rendered
}

fn render_open(
    db: &RawDb,
    source_id: &str,
    range: RawRange<'_>,
    blob_bytes: u64,
    target: RenderTarget<'_>,
) -> Result<RenderOutcome> {
    let pin = db.pin().expect("a reader is pinned at open").clone();
    let (all_chats, scan) = block_on(async {
        let messages =
            datalib_etl::doltlite_raw::load_payloads_with_id(db.pool(), "sms_messages").await?;
        let calls =
            datalib_etl::doltlite_raw::load_payloads_with_id(db.pool(), "sms_calls").await?;
        let scan = scan_diff(db.pool(), range.cursor, &pin).await?;
        // No early return on an empty store: a reset one still has to
        // name the conversations it lost, so their documents go.
        anyhow::Ok((build_chats(source_id, &messages, &calls), scan))
    })?;
    let projection = |_: &NormalizedChat| SMS_BLOB_PROJECTION;
    let blobs = ChatBlobs::new(db.pool(), Some(db.cas().pool()), &projection);
    render_changed_batched(
        &profile(),
        all_chats,
        scan,
        range,
        |id| ids::conversation(source_id, id).uuid,
        &blobs,
        blob_bytes,
        target,
    )
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
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

        // A text conversation has no title: it is named after whoever is
        // on the other end, by number where its key is one.
        let named_after = vec![NamedPerson {
            shown: display.clone(),
            handle: Handle::tel(&id),
        }];
        let conversation = ids::conversation(source_id, &id);
        chats.push(NormalizedChat {
            named_after,
            contacts: Vec::new(),
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
            let number = v.get("number").and_then(Value::as_str).unwrap_or("");
            NormalizedChatItem {
                message_uuid,
                author_handle: Handle::tel(number),
                author_kind: AuthorKind::Named,
                author_display: display.to_string(),
                date_ms,
                text: None,
                kind: ItemKind::System,
                attachments: Vec::new(),
                reactions: Vec::new(),
                labels: Vec::new(),
                system_note: Some(call_note(call_type, duration, display)),
                source_url: None,
                kind_label: None,
                source_ref: source_ref.clone(),
                is_aside: false,
                branch: Vec::new(),
                unread: false,
                recipients: Vec::new(),
                mentions: Vec::new(),
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
            let (author_handle, author_display) = if is_me {
                (None, "Me".to_string())
            } else {
                // A group MMS joins its numbers with `~`, which `tel`
                // refuses: the backup does not say which one sent it.
                let address = v.get("address").and_then(Value::as_str).unwrap_or("");
                (Handle::tel(address), display.to_string())
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
                author_handle,
                author_kind: if is_me {
                    AuthorKind::Account
                } else {
                    AuthorKind::Named
                },
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
                labels: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: None,
                source_ref: source_ref.clone(),
                is_aside: false,
                branch: Vec::new(),
                // Only an explicit `read="0"` on a message someone else
                // sent: an older store's rows carry no `read` at all.
                unread: !is_me && v.get("read").and_then(Value::as_bool) == Some(false),
                recipients: if is_me {
                    sent_to(v, display)
                } else {
                    Vec::new()
                },
                mentions: Vec::new(),
                problems: Vec::new(),
            }
        }
    }
}

/// Whom a text the account sent went to: the thread's other side, by
/// number. Drawn on no message, since the conversation already says who.
fn sent_to(v: &Value, display: &str) -> Vec<Recipient> {
    let address = v.get("address").and_then(Value::as_str).unwrap_or("");
    let numbers: Vec<&str> = address
        .split('~')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .collect();
    // A group MMS joins its numbers with `~`, and its contact name is
    // theirs together, so each number shows as itself.
    let shown = |number: &str| if numbers.len() == 1 { display } else { number }.to_string();
    numbers
        .iter()
        .map(|number| Recipient {
            role: RecipientRole::To,
            display: shown(number),
            handle: Handle::tel(number),
            drawn: false,
        })
        .collect()
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

    /// An incoming text names its sender's number as a `tel:` handle, so
    /// a contact linked in WhatsApp or Messages reaches it too. Mine, a
    /// number without its country code and a group MMS name nobody.
    #[test]
    fn an_incoming_message_carries_its_senders_number() {
        let row = |id: &str, kind: &str, address: &str, is_me: bool| {
            json!({"id":id,"kind":kind,"conversation_key":address,"conversation_display":"Jean-Luc Picard",
                   "date":1778277198761i64,"is_me":is_me,"address":address,"body":id,"attachments":[]})
        };
        let messages = vec![
            row("theirs", "sms", "+1 (555) 012-3456", false),
            row("mine", "sms", "+15550123456", true),
            row("local", "sms", "5550123456", false),
            row("group", "mms", "+15550123456~+15550109876", false),
        ];
        let chats = build_chats("sms", &with_ids(&messages), &[]);
        let handle_of = |id: &str| {
            chats
                .iter()
                .flat_map(|c| &c.buckets)
                .flat_map(|b| &b.items)
                .find(|i| i.source_ref.as_ref().unwrap().native_id == id)
                .unwrap()
                .author_handle
                .as_ref()
                .map(|h| h.as_str().to_string())
        };
        assert_eq!(handle_of("theirs").as_deref(), Some("tel:+15550123456"));
        assert_eq!(handle_of("mine"), None);
        assert_eq!(handle_of("local"), None);
        assert_eq!(handle_of("group"), None);
    }

    /// A call log names the other side's number as a `tel:` handle, so
    /// `with:` them finds it as it finds their texts. A number without
    /// its country code names nobody.
    #[test]
    fn a_call_carries_the_other_sides_number() {
        let call = |id: &str, number: &str| {
            json!({"id":id,"kind":"call","conversation_key":number,"conversation_display":"Deanna Troi",
                   "date":12600217800000i64,"call_type":"outgoing","number":number,"duration":240})
        };
        let calls = vec![
            call("intl", "+1 (202) 555-0103"),
            call("local", "2025550103"),
        ];
        let chats = build_chats("sms", &[], &with_ids(&calls));
        let handle_of = |id: &str| {
            chats
                .iter()
                .flat_map(|c| &c.buckets)
                .flat_map(|b| &b.items)
                .find(|i| i.source_ref.as_ref().unwrap().native_id == id)
                .unwrap()
                .author_handle
                .as_ref()
                .map(|h| h.as_str().to_string())
        };
        assert_eq!(handle_of("intl").as_deref(), Some("tel:+12025550103"));
        assert_eq!(handle_of("local"), None);
    }

    /// A text the account sent is addressed to the thread's other side,
    /// so a thread only the account wrote in still names them. The
    /// conversation already says who, so no To line is drawn.
    #[test]
    fn a_sent_text_is_addressed_to_the_threads_other_side() {
        let row = |id: &str, kind: &str, address: &str, is_me: bool| {
            json!({"id":id,"kind":kind,"conversation_key":address,"conversation_display":"Geordi La Forge",
                   "date":12600270000000i64,"is_me":is_me,"address":address,"body":id,"attachments":[]})
        };
        let messages = vec![
            row("sent", "sms", "+1 (202) 555-0104", true),
            row("received", "sms", "+12025550104", false),
            row("local", "sms", "2025550104", true),
            row("group", "mms", "+12025550104~+12025550105", true),
        ];
        let chats = build_chats("sms", &with_ids(&messages), &[]);
        let recipients_of = |id: &str| -> Vec<(String, Option<String>, bool)> {
            chats
                .iter()
                .flat_map(|c| &c.buckets)
                .flat_map(|b| &b.items)
                .find(|i| i.source_ref.as_ref().unwrap().native_id == id)
                .unwrap()
                .recipients
                .iter()
                .map(|r| {
                    assert_eq!(r.role, RecipientRole::To);
                    (
                        r.display.clone(),
                        r.handle.as_ref().map(|h| h.as_str().to_string()),
                        r.drawn,
                    )
                })
                .collect()
        };
        let to = |display: &str, handle: Option<&str>| {
            (display.to_string(), handle.map(str::to_string), false)
        };
        assert_eq!(
            recipients_of("sent"),
            [to("Geordi La Forge", Some("tel:+12025550104"))]
        );
        assert_eq!(recipients_of("received"), []);
        assert_eq!(recipients_of("local"), [to("Geordi La Forge", None)]);
        assert_eq!(
            recipients_of("group"),
            [
                to("+12025550104", Some("tel:+12025550104")),
                to("+12025550105", Some("tel:+12025550105")),
            ]
        );
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
        assert_eq!(
            me.author_kind,
            AuthorKind::Account,
            "a stand-in, not a name"
        );
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
