//! Read the mirrored `chat.db` out of the raw store and render it
//! through chat-common.
//!
//! The store is `chat.db` table for table, so rows are tied together
//! the way Messages ties them — by rowid, with `chat_message_join`
//! putting a message in its chat. Identity comes from the guids beside
//! the rowids: a chat is its `chat.guid`, a message its `message.guid`,
//! both Apple-issued and the same in every copy of one account's
//! database. Attachments are named, not copied: the files sit under
//! `~/Library/Messages/Attachments/`, which a picked `chat.db` grants no
//! access to, so each renders as a placeholder carrying its path.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw;
use datalib_etl::periodize::Period;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{Bucket, Buckets, RenderProfile, ENTITY_KIND_CONVERSATION};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction, UpstreamRef,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Inputs, RawRange};
use datalib_id::{composite_key, entity_id_str, IdNamespace};
use datalib_schema::providers::Provider;
use datalib_time::RecordStampPrecision;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::typedstream::attributed_body_text;

/// v2: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout).
pub const RENDER_VERSION: u32 = 2;

pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

pub const KIND_MESSAGE: &str = "message";
/// A tapback: a message row in `chat.db`, keyed on its own guid.
pub const KIND_REACTION: &str = "reaction";
/// One period of a chat: keyed on `(chat_guid, period_key)`.
pub const KIND_DOCUMENT: &str = "document";

/// The tables whose diff names a changed chat: the chat itself, a
/// message, or a join that puts a message in a chat or a file on a
/// message. Handles and attachments reach their chats through the rows
/// each chat declares it read.
const FORWARD_TABLES: &[&str] = &[
    "chat",
    "chat_message_join",
    "message",
    "message_attachment_join",
];

/// Milliseconds between the unix epoch and Apple's (2001-01-01).
const APPLE_EPOCH_MS: i64 = 978_307_200_000;

/// `date_ms` is the item's `date_ms`, so the stamp in the id is the
/// row's; a chat's and a document's ids carry none, their rows' stamps
/// being derived from their items.
fn uuid(source_id: &str, entity_kind: &str, natural_key: &str, date_ms: Option<i64>) -> String {
    entity_id_str(
        IdNamespace::AppleMessages,
        source_id,
        None,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

pub fn chat_uuid(source_id: &str, chat_guid: &str) -> String {
    uuid(source_id, ENTITY_KIND_CONVERSATION, chat_guid, None)
}

pub fn message_uuid(source_id: &str, message_guid: &str, date_ms: Option<i64>) -> String {
    uuid(source_id, KIND_MESSAGE, message_guid, date_ms)
}

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: STAMP_PRECISION,
        provider: Provider::AppleMessages,
        source_label: "Messages".to_string(),
        chat_kind: "Messages Chat".to_string(),
        message_kind: "Messages Message".to_string(),
        reaction_kind: "Messages Tapback".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

/// What one render pass did: the cursor to stamp and every chat it
/// declared.
#[derive(Debug, Default)]
pub struct RenderOutcome {
    pub rendered: usize,
    pub skipped: usize,
    pub new_head: Option<String>,
    pub buckets: Buckets,
}

pub fn render(
    raw_dir: &Path,
    out_root: &Path,
    source_id: &str,
    period: Period,
    progress: &Progress,
    range: RawRange<'_>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderOutcome> {
    let db_path = doltlite_raw::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderOutcome::default());
    }
    let (all_chats, forward, new_head) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // No commit means nothing has been committed to render, which
            // is emptiness rather than a reason to read the working set.
            let Some(reader) = doltlite_raw::open_reader(&db_path, range.pin).await? else {
                return Ok((Vec::new(), None, None));
            };
            let loaded = load(reader.pool(), source_id, period, range, reader.pin()).await;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            reader.close().await;
            loaded
        })
    })?;

    // The driver names stale buckets by chat uuid; the chats are by guid.
    let by_uuid: HashMap<String, &str> = all_chats
        .iter()
        .map(|c| (c.chat_uuid.clone(), c.id.as_str()))
        .collect();
    let narrowed = range.narrow_by(forward.as_ref(), |key| {
        by_uuid.get(key).map(|g| g.to_string())
    });
    // Named chats first, with no documents: one this run looked at that
    // has no message left builds no chat, and chat-common never sees it.
    // The rendered ones follow and replace that.
    let mut buckets: Buckets = narrowed
        .render
        .iter()
        .flatten()
        .map(|guid| chat_uuid(source_id, guid))
        .chain(narrowed.gone.iter().cloned())
        .map(|key| Bucket {
            key,
            inputs: Vec::new(),
        })
        .collect();
    let total = all_chats.len();
    let chats: Vec<NormalizedChat> = match &narrowed.render {
        None => all_chats,
        Some(live) => all_chats
            .into_iter()
            .filter(|c| live.contains(&c.id))
            .collect(),
    };
    let summary = datalib_etl_chat_common::render_all(
        &profile(),
        &chats,
        out_root,
        source_id,
        &HashMap::new(),
        progress,
        on_doc_complete,
    )?;
    buckets.extend(summary.buckets);
    Ok(RenderOutcome {
        rendered: summary.docs_rendered,
        skipped: total - chats.len(),
        new_head,
        buckets,
    })
}

type Loaded = (Vec<NormalizedChat>, Option<HashSet<String>>, Option<String>);

/// Every chat at the pinned commit, the guids of the chats the diff
/// since the cursor names (`None` renders everything), and the commit.
async fn load(
    pool: &SqlitePool,
    source_id: &str,
    period: Period,
    range: RawRange<'_>,
    pin: &datalib_etl::pin::Pin,
) -> Result<Loaded> {
    let handles: HashMap<i64, String> = sqlx::query("SELECT ROWID, id FROM pinned_handle")
        .fetch_all(pool)
        .await
        .context("select handle")?
        .iter()
        .map(|r| (r.get("ROWID"), r.get("id")))
        .collect();

    let mut chats: Vec<ChatBuild> = Vec::new();
    let mut chat_idx: HashMap<i64, usize> = HashMap::new();
    let chat_rows = sqlx::query(
        "SELECT ROWID, guid, chat_identifier, display_name FROM pinned_chat ORDER BY ROWID",
    )
    .fetch_all(pool)
    .await
    .context("select chat")?;
    for r in &chat_rows {
        let rowid: i64 = r.get("ROWID");
        let inputs = Inputs::default();
        inputs.read("chat", &rowid.to_string());
        chat_idx.insert(rowid, chats.len());
        chats.push(ChatBuild {
            guid: r.get("guid"),
            identifier: r.get::<Option<String>, _>("chat_identifier"),
            display_name: r.get::<Option<String>, _>("display_name"),
            participants: Vec::new(),
            items: Vec::new(),
            reactions: Vec::new(),
            inputs,
        });
    }

    let members = sqlx::query("SELECT chat_id, handle_id FROM pinned_chat_handle_join")
        .fetch_all(pool)
        .await
        .context("select chat_handle_join")?;
    for r in &members {
        let (chat_id, handle_id): (i64, i64) = (r.get("chat_id"), r.get("handle_id"));
        if let Some(&idx) = chat_idx.get(&chat_id) {
            let chat = &mut chats[idx];
            chat.inputs
                .read("chat_handle_join", &format!("{chat_id}|{handle_id}"));
            chat.inputs.read("handle", &handle_id.to_string());
            if let Some(id) = handles.get(&handle_id) {
                chat.participants.push(id.clone());
            }
        }
    }

    let mut chat_of_message: HashMap<i64, usize> = HashMap::new();
    let joins = sqlx::query("SELECT chat_id, message_id FROM pinned_chat_message_join")
        .fetch_all(pool)
        .await
        .context("select chat_message_join")?;
    for r in &joins {
        let (chat_id, message_id): (i64, i64) = (r.get("chat_id"), r.get("message_id"));
        if let Some(&idx) = chat_idx.get(&chat_id) {
            chats[idx]
                .inputs
                .read("chat_message_join", &format!("{chat_id}|{message_id}"));
            chat_of_message.insert(message_id, idx);
        }
    }

    let mut attachments: HashMap<i64, Vec<NormalizedAttachment>> = HashMap::new();
    let files = sqlx::query(
        "SELECT j.message_id, a.ROWID AS attachment_id, a.filename, a.mime_type, \
                a.transfer_name, a.total_bytes \
           FROM pinned_message_attachment_join j \
           JOIN pinned_attachment a ON a.ROWID = j.attachment_id",
    )
    .fetch_all(pool)
    .await
    .context("select attachment")?;
    for r in &files {
        let (message_id, attachment_id): (i64, i64) = (r.get("message_id"), r.get("attachment_id"));
        let Some(&idx) = chat_of_message.get(&message_id) else {
            continue;
        };
        let inputs = &chats[idx].inputs;
        inputs.read(
            "message_attachment_join",
            &format!("{message_id}|{attachment_id}"),
        );
        inputs.read("attachment", &attachment_id.to_string());
        let filename: Option<String> = r.get("filename");
        attachments
            .entry(message_id)
            .or_default()
            .push(NormalizedAttachment {
                rel_path: None,
                file_name: r.get::<Option<String>, _>("transfer_name").or_else(|| {
                    filename
                        .as_deref()
                        .and_then(|p| p.rsplit('/').next())
                        .map(str::to_string)
                }),
                mime_type: r.get("mime_type"),
                byte_len: r.get::<Option<i64>, _>("total_bytes").filter(|n| *n > 0),
                source_url: filename,
                ref_id: None,
            });
    }

    let messages = sqlx::query(
        "SELECT ROWID, guid, text, attributedBody, date, is_from_me, handle_id, item_type, \
                group_action_type, group_title, associated_message_guid, \
                associated_message_type, associated_message_emoji \
           FROM pinned_message ORDER BY date, ROWID",
    )
    .fetch_all(pool)
    .await
    .context("select message")?;
    for r in &messages {
        let rowid: i64 = r.get("ROWID");
        let Some(&idx) = chat_of_message.get(&rowid) else {
            tracing::debug!(message_rowid = rowid, "message: in no chat; dropping");
            continue;
        };
        let chat = &mut chats[idx];
        chat.inputs.read("message", &rowid.to_string());
        let handle_id: i64 = r.get("handle_id");
        if handle_id > 0 {
            chat.inputs.read("handle", &handle_id.to_string());
        }
        let from_me: i64 = r.get("is_from_me");
        let (author_id, author_display) = if from_me == 1 {
            ("me".to_string(), "Me".to_string())
        } else {
            let id = handles
                .get(&handle_id)
                .or(chat.identifier.as_ref())
                .cloned()
                .unwrap_or_else(|| "?".to_string());
            (id.clone(), id)
        };
        let guid: String = r.get("guid");
        let date_ms = apple_date_ms(r.get("date"));
        let tapback: i64 = r.get("associated_message_type");
        if (2000..4000).contains(&tapback) {
            chat.reactions.push(Tapback {
                target: r
                    .get::<Option<String>, _>("associated_message_guid")
                    .map(|s| target_guid(&s).to_string())
                    .unwrap_or_default(),
                removal: tapback >= 3000,
                reaction: NormalizedReaction {
                    reaction_uuid: uuid(source_id, KIND_REACTION, &guid, date_ms),
                    reactor_display: author_display,
                    emoji: tapback_emoji(tapback % 1000, r.get("associated_message_emoji")),
                    date_ms,
                    source_ref: Some(UpstreamRef::new(KIND_REACTION, guid)),
                },
            });
            continue;
        }
        let attachments = attachments.remove(&rowid).unwrap_or_default();
        let item_type: i64 = r.get("item_type");
        let system_note = (item_type != 0)
            .then(|| group_event(item_type, r.get("group_action_type"), r.get("group_title")));
        let kind = match (&system_note, attachments.is_empty()) {
            (Some(_), _) => ItemKind::System,
            (None, false) => ItemKind::Attachment,
            (None, true) => ItemKind::Text,
        };
        chat.items.push(NormalizedChatItem {
            message_uuid: message_uuid(source_id, &guid, date_ms),
            author_id,
            author_display,
            date_ms,
            text: body_text(r.get("text"), r.get("attributedBody"), &guid),
            kind,
            attachments,
            reactions: Vec::new(),
            system_note,
            source_url: None,
            kind_label: None,
            source_ref: Some(UpstreamRef::new(KIND_MESSAGE, guid)),
            is_aside: false,
            problems: Vec::new(),
        });
    }

    let forward = changed_rows(pool, range, pin, FORWARD_TABLES)
        .await?
        .map(|changed| forward_chats(&changed, &chats, &chat_idx, &chat_of_message));
    let out = chats
        .into_iter()
        .map(|c| c.finish(source_id, period))
        .collect();
    Ok((out, forward, Some(pin.commit().to_string())))
}

/// The guids of the chats the diff names, at the pinned commit. A chat
/// the diff names that is gone from it cannot be mapped here; the driver
/// finds it through the row it declared, and `narrow_by` reports it gone.
fn forward_chats(
    changed: &HashMap<String, HashSet<String>>,
    chats: &[ChatBuild],
    chat_idx: &HashMap<i64, usize>,
    chat_of_message: &HashMap<i64, usize>,
) -> HashSet<String> {
    let keys = |table: &str| changed.get(table).into_iter().flatten();
    let first = |key: &str| key.split('|').next().and_then(|s| s.parse::<i64>().ok());
    let by_chat = keys("chat")
        .chain(keys("chat_message_join"))
        .filter_map(|k| first(k))
        .filter_map(|c| chat_idx.get(&c));
    let by_message = keys("message")
        .chain(keys("message_attachment_join"))
        .filter_map(|k| first(k))
        .filter_map(|m| chat_of_message.get(&m));
    by_chat
        .chain(by_message)
        .map(|&idx| chats[idx].guid.clone())
        .collect()
}

struct ChatBuild {
    guid: String,
    identifier: Option<String>,
    display_name: Option<String>,
    participants: Vec<String>,
    items: Vec<NormalizedChatItem>,
    reactions: Vec<Tapback>,
    inputs: Inputs,
}

struct Tapback {
    target: String,
    removal: bool,
    reaction: NormalizedReaction,
}

impl ChatBuild {
    fn finish(mut self, source_id: &str, period: Period) -> NormalizedChat {
        // Tapbacks, in date order: an add lands on its message, a
        // removal takes the same person's same tapback off it again.
        let index: HashMap<String, usize> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| Some((it.source_ref.as_ref()?.native_id.clone(), i)))
            .collect();
        for t in self.reactions {
            let Some(&i) = index.get(&t.target) else {
                continue;
            };
            let on = &mut self.items[i].reactions;
            if t.removal {
                if let Some(pos) = on.iter().rposition(|r| {
                    r.reactor_display == t.reaction.reactor_display && r.emoji == t.reaction.emoji
                }) {
                    on.remove(pos);
                }
            } else {
                on.push(t.reaction);
            }
        }

        let mut by_period: HashMap<String, Vec<NormalizedChatItem>> = HashMap::new();
        for item in self.items {
            let key = match item.date_ms {
                Some(ms) => period.key_for_ms(ms),
                None => period.key_for_undated(),
            };
            by_period.entry(key).or_default().push(item);
        }
        let mut keys: Vec<String> = by_period.keys().cloned().collect();
        keys.sort();
        let buckets = keys
            .into_iter()
            .map(|period_key| {
                let key = composite_key(&[&self.guid, &period_key]);
                NormalizedDoc {
                    markdown_uuid: uuid(source_id, KIND_DOCUMENT, &key, None),
                    source_ref: Some(UpstreamRef::new(KIND_DOCUMENT, key)),
                    items: by_period.remove(&period_key).unwrap_or_default(),
                    period_key,
                    orphan_reactions: Vec::new(),
                }
            })
            .collect();
        let display = self
            .display_name
            .filter(|s| !s.trim().is_empty())
            .or_else(|| (!self.participants.is_empty()).then(|| self.participants.join(", ")))
            .or(self.identifier)
            .unwrap_or_else(|| self.guid.clone());
        NormalizedChat {
            inputs: self.inputs.declared(),
            path_prefix: None,
            chat_uuid: chat_uuid(source_id, &self.guid),
            external_id: Some(self.guid.clone()),
            id: self.guid,
            display,
            title: None,
            account: None,
            author: None,
            project: None,
            upstream_account: None,
            source_url: None,
            org_uuid: None,
            org_name: None,
            buckets,
        }
    }
}

/// `message.date`: nanoseconds since 2001 in any database written this
/// decade, seconds in one older than that. A zero is "no date".
fn apple_date_ms(date: Option<i64>) -> Option<i64> {
    let v = date.filter(|d| *d != 0)?;
    let ms = if v.abs() < 100_000_000_000 {
        v * 1000
    } else {
        v / 1_000_000
    };
    Some(ms + APPLE_EPOCH_MS)
}

/// The body: `text` where the row still has one, else the string inside
/// `attributedBody`. An attachment's placeholder character is not text.
fn body_text(text: Option<String>, body: Option<Vec<u8>>, guid: &str) -> Option<String> {
    let raw = match text.filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => {
            let body = body?;
            attributed_body_text(&body).or_else(|| {
                tracing::warn!(
                    guid,
                    bytes = body.len(),
                    "attributedBody: not a typedstream archive"
                );
                None
            })?
        }
    };
    let cleaned = raw.replace('\u{fffc}', "").trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `associated_message_guid` is `p:<part>/<guid>` or `bp:<guid>`.
fn target_guid(s: &str) -> &str {
    s.rsplit_once('/')
        .map(|(_, g)| g)
        .or_else(|| s.strip_prefix("bp:"))
        .unwrap_or(s)
}

fn tapback_emoji(kind: i64, custom: Option<String>) -> String {
    match kind {
        0 => "❤️",
        1 => "👍",
        2 => "👎",
        3 => "😂",
        4 => "‼️",
        5 => "❓",
        _ => return custom.unwrap_or_else(|| "?".to_string()),
    }
    .to_string()
}

fn group_event(item_type: i64, action: i64, title: Option<String>) -> String {
    match (item_type, action) {
        (1, 0) => "Added someone to the group".to_string(),
        (1, 1) => "Removed someone from the group".to_string(),
        (2, _) => format!("Named the group “{}”", title.unwrap_or_default()),
        (3, _) => "Left the group".to_string(),
        _ => format!("Messages event (item_type {item_type})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_apple_epoch_in_nanoseconds_or_seconds() {
        // 2026-04-05T09:15:00Z, as a modern row and as a pre-2011 one.
        assert_eq!(
            apple_date_ms(Some(797_073_300_000_000_000)),
            Some(1_775_380_500_000)
        );
        assert_eq!(apple_date_ms(Some(797_073_300)), Some(1_775_380_500_000));
        assert_eq!(apple_date_ms(Some(0)), None);
        assert_eq!(apple_date_ms(None), None);
    }

    #[test]
    fn tapback_targets_and_emoji() {
        assert_eq!(target_guid("p:0/ABC-123"), "ABC-123");
        assert_eq!(target_guid("bp:ABC-123"), "ABC-123");
        assert_eq!(tapback_emoji(0, None), "❤️");
        assert_eq!(tapback_emoji(6, Some("🖖".into())), "🖖");
    }

    #[test]
    fn a_placeholder_only_body_is_no_text() {
        assert_eq!(body_text(Some("\u{fffc}".into()), None, "g"), None);
        assert_eq!(
            body_text(Some(" hi ".into()), None, "g").as_deref(),
            Some("hi")
        );
        assert_eq!(body_text(None, None, "g"), None);
    }
}
