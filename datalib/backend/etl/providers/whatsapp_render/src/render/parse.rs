//! Read the mirrored msgstore tables out of the raw doltlite store and
//! assemble `Vec<NormalizedChat>` for chat-common's renderer.
//!
//! The store is msgstore.db table for table, so every reference between
//! tables is a rowid (`message.chat_row_id -> chat._id -> jid._id`). This
//! is where those are resolved to the natural keys the uuids are minted
//! from: a chat is its `jid.raw_string`, a message is
//! `(chat_jid, key_id, from_me)`. Rowids are stable between backups of one
//! phone, but they are still not identity.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
use datalib_etl::blob_cas::{self, BlobBundle};
use datalib_etl::doltlite_raw::table_exists;
use datalib_etl::periodize::Period;
use datalib_etl_chat_common::types::UpstreamRef;
use datalib_etl_chat_common::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_handle::Handle;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use super::ids;

/// SQL projection resolving an attachment's `ref_id` — the file's
/// blake3, which render stamps onto each `NormalizedAttachment` — to
/// its CAS entry. Consumed by [`BlobBundle::load_many`] from the per-chat
/// load below. A row exists only for a file the scan actually saw, so
/// a half-extracted `Media/` tree simply yields no bytes.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT blake3 AS ref_id, blake3,
           mime_type AS content_type,
           relative_path AS upstream_name
      FROM wa_media_files
     WHERE blake3 IN ({placeholders})";

/// What `parse` returns to render: the chat tree plus a per-chat `BlobBundle`
/// carrying every attachment's bytes pre-loaded from the sibling CAS — the
/// synchronous bag the chat-common renderer reads at `materialize_to_dir`
/// time, mirroring slack's shape.
#[derive(Default)]
pub struct ParsedWhatsApp {
    pub chats: Vec<NormalizedChat>,
    pub blobs_by_chat: HashMap<String, BlobBundle>,
}

/// The natural key of a message, resolved from its rowid.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MsgKey {
    pub chat_jid: String,
    pub key_id: String,
    pub from_me: i64,
}

pub fn parse(
    raw_dir: &Path,
    period: Period,
    source_id: &str,
    range: RawRange<'_>,
) -> Result<ParsedWhatsApp> {
    let db_path = datalib_etl::doltlite_raw::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(ParsedWhatsApp::default());
    }
    // Bridge to sync sqlx code: this fn is called from a sync context
    // (render phase). We need to spin up a tokio runtime since sqlx
    // is async-only.
    tokio::task::block_in_place(|| {
        let rt = tokio::runtime::Handle::try_current();
        match rt {
            Ok(handle) => handle.block_on(parse_async(&db_path, period, source_id, range)),
            Err(_) => tokio::runtime::Runtime::new()?
                .block_on(parse_async(&db_path, period, source_id, range)),
        }
    })
}

async fn parse_async(
    db_path: &Path,
    period: Period,
    source_id: &str,
    range: RawRange<'_>,
) -> Result<ParsedWhatsApp> {
    // Pinned at open: the driver's pin when it made one, else HEAD. No
    // commit means nothing has been committed to render, which is
    // emptiness rather than a reason to read the working set.
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open {}", db_path.display()))?
    else {
        return Ok(ParsedWhatsApp::default());
    };
    let pool: SqlitePool = reader.pool().clone();

    let jids = load_jids(&pool).await?;
    let names = JidNames::load(&pool, &jids).await?;

    // 1) Chats, with their display label. Group chats use `subject`; 1:1
    //    chats fall back to what the JID resolves to.
    //    `last_read_message_row_id` is the read mark: an incoming message
    //    with a greater `_id` is unread. On a real backup that rule gives
    //    back each chat's `unseen_message_count` exactly; a chat nothing
    //    was read in points it at the seed row, `_id` 1.
    let has_read_mark =
        datalib_etl::doltlite_raw::column_exists(&pool, "chat", "last_read_message_row_id").await?;
    let chat_sql = if has_read_mark {
        "SELECT _id, jid_row_id, subject, last_read_message_row_id FROM chat ORDER BY _id"
    } else {
        "SELECT _id, jid_row_id, subject, NULL AS last_read_message_row_id FROM chat ORDER BY _id"
    };
    let chat_rows = sqlx::query(chat_sql)
        .fetch_all(&pool)
        .await
        .context("select chat")?;
    let mut chats: Vec<ChatHeader> = Vec::with_capacity(chat_rows.len());
    let mut chat_idx_by_rowid: HashMap<i64, usize> = HashMap::with_capacity(chat_rows.len());
    for r in &chat_rows {
        let rowid: i64 = r.get("_id");
        let jid_row_id: Option<i64> = r.get("jid_row_id");
        let Some(chat_jid) = jid_row_id.and_then(|i| jids.get(&i).cloned()) else {
            tracing::warn!(chat_rowid = rowid, "chat: jid_row_id not in jid; dropping");
            continue;
        };
        // Every row this chat reads, recorded as it is read: its own
        // row and its JID here, each message and media row below, each
        // name lookup as `JidNames` makes it.
        let inputs = Inputs::default();
        inputs.read("chat", &rowid.to_string());
        if let Some(jid_row_id) = jid_row_id {
            inputs.read("jid", &jid_row_id.to_string());
        }
        let subject: Option<String> = r.get("subject");
        let display = subject
            .clone()
            .unwrap_or_else(|| names.label(&chat_jid, &inputs));
        chat_idx_by_rowid.insert(rowid, chats.len());
        chats.push(ChatHeader {
            chat_jid,
            display,
            last_read_rowid: r.get("last_read_message_row_id"),
            items_by_period: HashMap::new(),
            person_jids: Vec::new(),
            inputs,
        });
    }

    // 2) Messages. Read once into natural keys; the rowid map is what the
    //    child tables below join through.
    let msg_rows = sqlx::query(
        "SELECT _id, chat_row_id, key_id, from_me, sender_jid_row_id, timestamp, \
                message_type, text_data \
         FROM message ORDER BY chat_row_id, sort_id, timestamp, key_id",
    )
    .fetch_all(&pool)
    .await
    .context("select message")?;
    let mut msg_key_by_rowid: HashMap<i64, MsgKey> = HashMap::with_capacity(msg_rows.len());
    let mut msg_chat_by_rowid: HashMap<i64, usize> = HashMap::with_capacity(msg_rows.len());
    let mut seen_keys: HashSet<MsgKey> = HashSet::with_capacity(msg_rows.len());
    for r in &msg_rows {
        let rowid: i64 = r.get("_id");
        let chat_row_id: i64 = r.get("chat_row_id");
        let key_id: String = r.get("key_id");
        let Some(&idx) = chat_idx_by_rowid.get(&chat_row_id) else {
            // Every msgstore ships a synthetic seed row at `_id=1` — an
            // Android schema artifact, not a message. Other orphan rows
            // still WARN, since a pruned chat or a corrupt source is worth
            // flagging.
            if chat_row_id == -1 && key_id == "-1" {
                tracing::debug!(message_rowid = rowid, "message: dropping msgstore seed row");
            } else {
                tracing::warn!(
                    message_rowid = rowid,
                    chat_row_id,
                    key_id,
                    "message: chat_row_id not in chat; dropping"
                );
            }
            continue;
        };
        let key = MsgKey {
            chat_jid: chats[idx].chat_jid.clone(),
            key_id,
            from_me: r.get("from_me"),
        };
        chats[idx].inputs.read("message", &rowid.to_string());
        // Two rows with one natural key would mint one uuid twice; keep
        // the first, the way the store's old unique key did.
        if !seen_keys.insert(key.clone()) {
            tracing::warn!(
                message_rowid = rowid,
                ?key,
                "message: duplicate natural key; dropping"
            );
            continue;
        }
        msg_key_by_rowid.insert(rowid, key);
        msg_chat_by_rowid.insert(rowid, idx);
    }

    // 3) Media joined to its `wa_media_files` row to pick up the blake3
    //    the ingest stored alongside the blob_cas put. That blake3 IS the
    //    `blob_refs.ref_id`, so chat-common can stream the bytes out at
    //    render time without render touching disk.
    let media_rows = sqlx::query(
        "SELECT m.message_row_id, m.file_path, m.mime_type, m.file_size, \
                m.media_caption, m.media_name, f.blake3 \
         FROM message_media m \
         LEFT JOIN wa_media_files f ON f.relative_path = m.file_path",
    )
    .fetch_all(&pool)
    .await
    .context("select message_media")?;
    let mut media_by_msg: HashMap<MsgKey, Vec<NormalizedAttachment>> = HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();
    for r in &media_rows {
        let message_row_id: i64 = r.get("message_row_id");
        let Some(key) = msg_key_by_rowid.get(&message_row_id) else {
            continue;
        };
        let file_path: Option<String> = r.get("file_path");
        let media_name: Option<String> = r.get("media_name");
        let mime_type: Option<String> = r.get("mime_type");
        let file_size: Option<i64> = r.get("file_size");
        let media_caption: Option<String> = r.get("media_caption");
        let ref_id: Option<String> = r.get("blake3");
        let inputs = &chats[msg_chat_by_rowid[&message_row_id]].inputs;
        inputs.read("message_media", &message_row_id.to_string());
        // The registry row is keyed by its blake3; one that resolved is
        // declared, one that did not reaches the chat by path through the
        // render-side scan when it arrives.
        if let Some(blake3) = ref_id.as_deref() {
            inputs.read("wa_media_files", blake3);
        }
        // `None` means either the file went missing between scan and put, or
        // the message's `file_path` didn't resolve to a `wa_media_files` row.
        // Either way the renderer's "(not yet fetched)" placeholder fires —
        // which is indistinguishable, in the rendered markdown, from a backup
        // whose `Media/` tree was never copied. The warning below is what tells
        // the two apart, so don't drop it: a join that silently matched nothing
        // is exactly how every attachment once rendered as a placeholder while
        // its bytes sat in the CAS.
        if ref_id.is_none() {
            if let Some(p) = file_path.as_deref() {
                unresolved.push(p.to_string());
            }
        }
        media_by_msg
            .entry(key.clone())
            .or_default()
            .push(NormalizedAttachment {
                rel_path: None,
                file_name: media_name.or_else(|| {
                    file_path
                        .as_deref()
                        .and_then(|p| p.rsplit('/').next())
                        .map(str::to_string)
                }),
                mime_type,
                byte_len: file_size,
                source_url: file_path.clone().or_else(|| media_caption.clone()),
                ref_id,
            });
    }
    if !unresolved.is_empty() {
        unresolved.sort();
        unresolved.dedup();
        tracing::warn!(
            event = "wa_media_unresolved",
            count = unresolved.len(),
            total_media = media_rows.len(),
            examples = %unresolved.iter().take(3).cloned().collect::<Vec<_>>().join(", "),
            "message_media.file_path matched no wa_media_files row; \
             those attachments render as placeholders",
        );
    }

    // 4) Reactions: an add-on row keyed like a message, plus its emoji.
    let react_rows = sqlx::query(
        "SELECT a._id, a.chat_row_id, a.key_id, a.from_me, a.sender_jid_row_id, \
                a.parent_message_row_id, a.timestamp, r.reaction \
         FROM message_add_on a \
         JOIN message_add_on_reaction r ON r.message_add_on_row_id = a._id \
         WHERE a.parent_message_row_id IS NOT NULL",
    )
    .fetch_all(&pool)
    .await
    .context("select message_add_on")?;
    let mut reactions_by_parent: HashMap<MsgKey, Vec<NormalizedReaction>> = HashMap::new();
    for r in &react_rows {
        let Some(parent) = msg_key_by_rowid.get(&r.get::<i64, _>("parent_message_row_id")) else {
            continue;
        };
        let chat_row_id: Option<i64> = r.get("chat_row_id");
        let Some(&chat_idx) = chat_row_id.and_then(|i| chat_idx_by_rowid.get(&i)) else {
            tracing::warn!("message_add_on: chat_row_id not in chat; dropping reaction");
            continue;
        };
        let chat_jid = chats[chat_idx].chat_jid.as_str();
        let inputs = &chats[chat_idx].inputs;
        let addon_row_id: i64 = r.get("_id");
        inputs.read("message_add_on", &addon_row_id.to_string());
        inputs.read("message_add_on_reaction", &addon_row_id.to_string());
        let key_id: String = r.get("key_id");
        let from_me: i64 = r.get::<Option<i64>, _>("from_me").unwrap_or(0);
        let sender_jid_row_id: Option<i64> = r.get("sender_jid_row_id");
        if let Some(i) = sender_jid_row_id {
            inputs.read("jid", &i.to_string());
        }
        // The author's rule: the account's own reaction names nobody, and
        // a 1:1 chat leaves the sender empty, as it does on a message, so
        // the chat JID is who reacted (a group's maps to no handle).
        let reactor: Option<String> = (from_me != 1).then(|| {
            sender_jid_row_id
                .and_then(|i| jids.get(&i))
                .map_or(chat_jid, String::as_str)
                .to_string()
        });
        let emoji: Option<String> = r.get("reaction");
        let timestamp: Option<i64> = r.get("timestamp");
        let reactor_display = match reactor.as_deref() {
            Some(j) => names.label(j, inputs),
            None => "Me".to_string(),
        };
        // A NULL `timestamp` column is "we don't know when", which is a
        // null `created_at` — not 1970.
        let id = ids::reaction(source_id, chat_jid, &key_id, from_me, timestamp);
        reactions_by_parent
            .entry(parent.clone())
            .or_default()
            .push(NormalizedReaction {
                reaction_uuid: id.uuid,
                reactor_handle: reactor.as_deref().and_then(|j| names.handle(j)),
                reactor_display,
                source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
                emoji: emoji.unwrap_or_else(|| "?".to_string()),
                date_ms: timestamp,
            });
        if let Some(j) = reactor {
            let people = &mut chats[chat_idx].person_jids;
            if !people.contains(&j) {
                people.push(j);
            }
        }
    }

    // 5) Walk messages, bucket by period, attach media + reactions.
    for r in &msg_rows {
        let Some(key) = msg_key_by_rowid.get(&r.get::<i64, _>("_id")) else {
            continue;
        };
        let idx = chat_idx_by_rowid[&r.get::<i64, _>("chat_row_id")];
        let sender_jid_row_id: Option<i64> = r.get("sender_jid_row_id");
        if let Some(i) = sender_jid_row_id {
            chats[idx].inputs.read("jid", &i.to_string());
        }
        let sender_jid = sender_jid_row_id.and_then(|i| jids.get(&i).cloned());
        if key.from_me == 0 {
            // 1:1 incoming: the chat JID is the sender, by definition.
            let author = sender_jid.clone().unwrap_or_else(|| key.chat_jid.clone());
            if !chats[idx].person_jids.contains(&author) {
                chats[idx].person_jids.push(author);
            }
        }
        let rowid: i64 = r.get("_id");
        let unread =
            key.from_me == 0 && chats[idx].last_read_rowid.is_some_and(|mark| rowid > mark);
        let item = build_item(
            source_id,
            key,
            sender_jid,
            r,
            &names,
            &chats[idx].inputs,
            media_by_msg.remove(key).unwrap_or_default(),
            reactions_by_parent.remove(key).unwrap_or_default(),
            unread,
        );
        let period_key = match r.get::<Option<i64>, _>("timestamp") {
            Some(ts) => period.key_for_ms(ts),
            // An undated message still has to be filed somewhere;
            // `key_for_undated` documents why that is the epoch
            // bucket and not a new `"undated"` key. Its `created_at`
            // is null regardless — bucketing and `created_at` answer
            // different questions.
            None => period.key_for_undated(),
        };
        chats[idx]
            .items_by_period
            .entry(period_key)
            .or_default()
            .push(item);
    }

    // 6) Materialize into NormalizedChat.
    let mut out: Vec<NormalizedChat> = Vec::with_capacity(chats.len());
    for ch in chats.into_iter().filter(|c| !c.items_by_period.is_empty()) {
        let chat_id = ids::chat(source_id, &ch.chat_jid);
        let mut keys: Vec<String> = ch.items_by_period.keys().cloned().collect();
        keys.sort();
        let mut buckets: Vec<NormalizedDoc> = Vec::with_capacity(keys.len());
        let mut items_by_period = ch.items_by_period;
        for k in keys {
            let mut items = items_by_period.remove(&k).unwrap_or_default();
            items.sort_by_key(|i| i.date_ms);
            let period = ids::period(source_id, &ch.chat_jid, &k);
            buckets.push(NormalizedDoc {
                orphan_reactions: Vec::new(),
                period_key: k.clone(),
                markdown_uuid: period.uuid,
                source_ref: Some(UpstreamRef::new(period.entity_kind, period.natural_key)),
                items,
            });
        }
        out.push(NormalizedChat {
            // Each author's address-book entry, read the way their label was.
            contacts: ch
                .person_jids
                .iter()
                .filter_map(|j| names.contact(j, source_id))
                .collect(),
            inputs: ch.inputs.declared(),
            path_prefix: None,
            id: ch.chat_jid.clone(),
            chat_uuid: chat_id.uuid,
            display: ch.display,
            author: None,
            account: None,
            project: None,
            external_id: Some(chat_id.natural_key),
            source_url: None,
            upstream_account: None,
            title: None,
            org_uuid: None,
            org_name: None,
            buckets,
        });
    }

    // 7) Per-chat BlobBundle: walk every attachment in every chat,
    //    collect the unique `ref_id`s, then load their bytes from the
    //    sibling CAS in one shot via ATTACHMENTS_PROJECTION_SQL. Same
    //    shape slack uses for per-thread bundles — render no longer
    //    has to open a CAS pool itself.
    let cas_pool = blob_cas::open_cas_for_render(db_path)
        .await
        .with_context(|| format!("open the blob store beside {}", db_path.display()))?;
    let refs = out.iter().map(|chat| {
        let refs = chat
            .buckets
            .iter()
            .flat_map(|bucket| &bucket.items)
            .flat_map(|item| &item.attachments)
            .filter_map(|att| att.ref_id.as_deref());
        (chat.id.clone(), refs)
    });
    let loaded =
        BlobBundle::load_many(&pool, cas_pool.as_ref(), ATTACHMENTS_PROJECTION_SQL, refs).await;
    if let Some(cas) = cas_pool {
        cas.close().await;
    }
    let mut blobs_by_chat = loaded?;
    blobs_by_chat.retain(|_, bundle| !bundle.is_empty() || bundle.has_missing());
    pool.close().await;

    Ok(ParsedWhatsApp {
        chats: out,
        blobs_by_chat,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_item(
    source_id: &str,
    key: &MsgKey,
    sender_jid: Option<String>,
    r: &sqlx::sqlite::SqliteRow,
    names: &JidNames,
    inputs: &Inputs,
    attachments: Vec<NormalizedAttachment>,
    reactions: Vec<NormalizedReaction>,
    unread: bool,
) -> NormalizedChatItem {
    let timestamp: Option<i64> = r.get("timestamp");
    let message_type: Option<i64> = r.get("message_type");
    let text_data: Option<String> = r.get("text_data");

    let author_display = if key.from_me == 1 {
        "Me".to_string()
    } else if let Some(j) = sender_jid.as_deref() {
        names.label(j, inputs)
    } else {
        // 1:1 incoming: the chat JID IS the sender, by definition.
        names.label(&key.chat_jid, inputs)
    };
    let author_handle = if key.from_me == 1 {
        None
    } else {
        names.handle(sender_jid.as_deref().unwrap_or(&key.chat_jid))
    };

    // WhatsApp message_type codes (Android schema):
    //   0  text
    //   1  image
    //   2  audio
    //   3  video
    //   9  document
    //  13  animated gif
    //  20  sticker
    //  >50 system events (chat_renamed etc.)
    // For the first cut: treat 1/2/3/9/13/20 + any attachment-present
    // case as Attachment; non-zero-without-attachment + system-range
    // codes as System; everything else as Text.
    let kind = if !attachments.is_empty() {
        ItemKind::Attachment
    } else {
        match message_type.unwrap_or(0) {
            0 => ItemKind::Text,
            mt if mt >= 50 => ItemKind::System,
            _ => ItemKind::Text,
        }
    };

    let id = ids::message(
        source_id,
        &key.chat_jid,
        &key.key_id,
        key.from_me,
        timestamp,
    );
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_handle,
        author_display,
        // A NULL `timestamp` column is "we don't know when", which is a
        // null `created_at` — not 1970. See
        // `docs/dev/data_architecture_parse_and_render.md` §6.
        date_ms: timestamp,
        text: text_data,
        kind,
        attachments,
        reactions,
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread,
        recipients: Vec::new(),
        problems: Vec::new(),
    }
}

struct ChatHeader {
    chat_jid: String,
    display: String,
    last_read_rowid: Option<i64>,
    items_by_period: HashMap<String, Vec<NormalizedChatItem>>,
    /// Who wrote or reacted in the chat, in the order they first did;
    /// the account itself is not among them.
    person_jids: Vec<String>,
    inputs: Inputs,
}

/// `jid._id -> raw_string`. A few seed rows carry a NULL `raw_string`;
/// `user@server` is spelled for those so a row still has a key.
async fn load_jids(pool: &SqlitePool) -> Result<HashMap<i64, String>> {
    let rows =
        sqlx::query("SELECT _id, coalesce(raw_string, user || '@' || server) AS jid FROM jid")
            .fetch_all(pool)
            .await
            .context("select jid")?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("_id"), r.get::<String, _>("jid")))
        .collect())
}

/// Who a JID is. A `…@lid` (linked id) is an opaque number;
/// `lid_display_name` may name it and `jid_map` may say which phone
/// number it stands for. Neither table is total — the measured backup
/// mapped 5 of 6 `@lid` chats and named none. The names a person sees
/// in WhatsApp come from `wa.db` (`wa_db_contacts`): the address-book
/// name, and the name someone gave themselves. So: address book, then
/// `lid_display_name`, then their own name, then the phone number
/// (`label_from_jid` makes it dialable), then the raw JID.
#[derive(Default)]
struct JidNames {
    display_name: HashMap<String, String>,
    phone_jid: HashMap<String, String>,
    /// `wa_contacts.display_name`, by jid: the phone's address book.
    book_name: HashMap<String, String>,
    /// `wa_contacts.wa_name`, by jid: the name the person set themselves.
    own_name: HashMap<String, String>,
    /// Every `wa_contacts` row for a jid, in `_id` order: what the
    /// provider's account of the person is built from.
    book: HashMap<String, Vec<serde_json::Value>>,
    /// Whether the store has `wa_db_contacts` to declare reads of.
    has_contacts: bool,
    /// `jid` string → its row id, so a name lookup can declare the
    /// `lid_display_name` and `jid_map` rows it consulted (both keyed by
    /// the linked id's `jid._id`), found or not.
    row_id: HashMap<String, i64>,
}

impl JidNames {
    async fn load(pool: &SqlitePool, jids: &HashMap<i64, String>) -> Result<Self> {
        let mut out = Self::default();
        for (rowid, jid) in jids {
            out.row_id.entry(jid.clone()).or_insert(*rowid);
        }
        // Older msgstore versions have neither LID table, and a backup may
        // come without wa.db: absent is "nothing to map", not a failed render.
        if table_exists(pool, "lid_display_name").await? {
            let rows = sqlx::query("SELECT lid_row_id, display_name FROM lid_display_name")
                .fetch_all(pool)
                .await
                .context("select lid_display_name")?;
            for r in &rows {
                let name: String = r.get("display_name");
                if let Some(lid) = jids.get(&r.get::<i64, _>("lid_row_id")) {
                    if !name.trim().is_empty() {
                        out.display_name.insert(lid.clone(), name);
                    }
                }
            }
        }
        if table_exists(pool, "wa_db_contacts").await? {
            out.has_contacts = true;
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT jid, rows FROM wa_db_contacts")
                    .fetch_all(pool)
                    .await
                    .context("select wa_db_contacts")?;
            for (jid, contact_rows) in rows {
                let contact_rows: Vec<serde_json::Value> =
                    serde_json::from_str(&contact_rows).unwrap_or_default();
                let first = |field: &str| {
                    contact_rows
                        .iter()
                        .filter_map(|r| r.get(field)?.as_str())
                        .map(str::trim)
                        .find(|n| !n.is_empty())
                        .map(String::from)
                };
                if let Some(n) = first("display_name") {
                    out.book_name.insert(jid.clone(), n);
                }
                if let Some(n) = first("wa_name") {
                    out.own_name.insert(jid.clone(), n);
                }
                out.book.insert(jid, contact_rows);
            }
        }
        if table_exists(pool, "jid_map").await? {
            let rows = sqlx::query("SELECT lid_row_id, jid_row_id FROM jid_map")
                .fetch_all(pool)
                .await
                .context("select jid_map")?;
            for r in &rows {
                let lid = jids.get(&r.get::<i64, _>("lid_row_id"));
                let jid = jids.get(&r.get::<i64, _>("jid_row_id"));
                if let (Some(lid), Some(jid)) = (lid, jid) {
                    out.phone_jid.insert(lid.clone(), jid.clone());
                }
            }
        }
        Ok(out)
    }

    /// A linked id's handle is its phone number's, where `jid_map` knows it.
    fn handle(&self, jid: &str) -> Option<Handle> {
        Handle::whatsapp_jid(self.phone_jid.get(jid).map_or(jid, String::as_str))
    }

    /// The person as the phone's address book has them: every name an
    /// entry gives, the name they gave themselves, their company and
    /// title, keyed by their number. `None` for a jid with no handle
    /// or no entry. The reads are the ones [`JidNames::label`] declares.
    fn contact(&self, jid: &str, source_id: &str) -> Option<NormalizedContact> {
        let handle = self.handle(jid)?;
        // Under the jid and, for a linked id, its number, as `label` reads.
        let phone = self.phone_jid.get(jid).map(String::as_str);
        let rows: Vec<&serde_json::Value> = [Some(jid), phone.filter(|p| *p != jid)]
            .into_iter()
            .flatten()
            .filter_map(|j| self.book.get(j))
            .flatten()
            .collect();
        if rows.is_empty() {
            return None;
        }
        let field = |row: &serde_json::Value, name: &str| {
            row.get(name)?
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        };
        let mut c = NormalizedContact::new(source_id, handle.as_str(), ContactKind::Person);
        let mut push_name = |name: Option<String>| {
            if let Some(name) = name {
                if !c.names.contains(&name) {
                    c.names.push(name);
                }
            }
        };
        for row in &rows {
            push_name(field(row, "display_name"));
        }
        for row in &rows {
            push_name(
                match (field(row, "given_name"), field(row, "family_name")) {
                    (Some(g), Some(f)) => Some(format!("{g} {f}")),
                    (g, f) => g.or(f),
                },
            );
        }
        for row in &rows {
            push_name(field(row, "wa_name"));
        }
        c.handles.push(ContactHandle::of(handle));
        c.org = rows.iter().find_map(|r| field(r, "company"));
        c.title = rows.iter().find_map(|r| field(r, "title"));
        Some(c)
    }

    fn label(&self, jid: &str, inputs: &Inputs) -> String {
        if let Some(rowid) = self.row_id.get(jid) {
            let rowid = rowid.to_string();
            inputs.read("lid_display_name", &rowid);
            inputs.read("jid_map", &rowid);
        }
        let phone = self.phone_jid.get(jid).map(String::as_str);
        let this_or_phone = [Some(jid), phone];
        if self.has_contacts {
            for j in this_or_phone.iter().flatten() {
                inputs.read("wa_db_contacts", j);
            }
        }
        let from = |names: &HashMap<String, String>| {
            this_or_phone
                .iter()
                .flatten()
                .find_map(|j| names.get(*j).cloned())
        };
        if let Some(name) = from(&self.book_name) {
            return name;
        }
        if let Some(name) = self.display_name.get(jid) {
            return name.clone();
        }
        if let Some(name) = from(&self.own_name) {
            return name;
        }
        let jid = match self.phone_jid.get(jid) {
            Some(phone) => {
                if let Some(rowid) = self.row_id.get(phone) {
                    inputs.read("jid", &rowid.to_string());
                }
                phone.as_str()
            }
            None => jid,
        };
        label_from_jid(jid)
    }
}

fn label_from_jid(jid: &str) -> String {
    if let Some((user, server)) = jid.split_once('@') {
        if (server.starts_with("s.whatsapp.net") || server.starts_with("c.us"))
            && !user.is_empty()
            && user.chars().all(|c| c.is_ascii_digit())
        {
            return format!("+{user}");
        }
    }
    jid.to_string()
}

#[cfg(test)]
mod jid_names_tests {
    use super::{Handle, Inputs, JidNames};

    /// The address book's account of a person: every name its entries
    /// give, keyed by the number, reached through a linked id too; a jid
    /// with no entry, or no handle, has none.
    #[test]
    fn an_address_book_entry_is_the_providers_account_of_the_person() {
        let mut names = JidNames {
            has_contacts: true,
            ..JidNames::default()
        };
        let riker = "17015550102@s.whatsapp.net".to_string();
        names.phone_jid.insert("8@lid".into(), riker.clone());
        names.book.insert(
            riker.clone(),
            vec![
                serde_json::json!({"display_name": "William Riker", "given_name": "William",
                    "family_name": "Riker", "wa_name": "Number One", "company": "Starfleet",
                    "title": " First Officer "}),
                serde_json::json!({"display_name": "Will Riker (Starfleet)", "wa_name": "Number One"}),
            ],
        );
        names.book.insert(
            "bridge-crew@g.us".into(),
            vec![serde_json::json!({"display_name": "Bridge"})],
        );
        // A row under the linked id itself counts as well as the number's.
        names
            .book
            .insert("8@lid".into(), vec![serde_json::json!({"wa_name": "Bill"})]);
        let c = names.contact("8@lid", "wa").expect("through the linked id");
        assert_eq!(c.key, "tel:+17015550102");
        assert_eq!(
            c.names,
            [
                "William Riker",
                "Will Riker (Starfleet)",
                "Bill",
                "Number One"
            ],
            "every distinct name once, the address book's first, from both jids"
        );
        assert_eq!(c.handles.len(), 1);
        assert_eq!(c.handles[0].handle, Handle::tel("+17015550102"));
        assert_eq!(c.org.as_deref(), Some("Starfleet"));
        assert_eq!(c.title.as_deref(), Some("First Officer"));
        assert_eq!(names.contact(&riker, "wa").map(|c| c.key), Some(c.key));
        assert!(
            names.contact("17015550109@s.whatsapp.net", "wa").is_none(),
            "no entry"
        );
        assert!(
            names.contact("bridge-crew@g.us", "wa").is_none(),
            "no handle"
        );
    }

    /// Most people in a current backup are a linked id, not a phone JID;
    /// without the map their messages would carry no handle at all.
    #[test]
    fn a_linked_id_takes_its_phone_numbers_handle() {
        let mut names = JidNames::default();
        names.phone_jid.insert(
            "1@lid".to_string(),
            "17015550101@s.whatsapp.net".to_string(),
        );
        let phone = Handle::tel("+17015550101");
        assert_eq!(names.handle("1@lid"), phone);
        assert_eq!(names.handle("17015550101@s.whatsapp.net"), phone);
        assert_eq!(names.handle("3@lid"), None);
        assert_eq!(names.handle("bridge-crew@g.us"), None);
    }

    /// The precedence the issue asked for: a learned name, else the
    /// phone number behind the linked id, else the raw JID — and a
    /// mapping that is missing must not turn into a blank.
    #[test]
    fn name_then_mapped_phone_then_raw_jid() {
        let mut names = JidNames::default();
        names.phone_jid.insert(
            "1@lid".to_string(),
            "17015550101@s.whatsapp.net".to_string(),
        );
        names.phone_jid.insert(
            "2@lid".to_string(),
            "17015550102@s.whatsapp.net".to_string(),
        );
        names
            .display_name
            .insert("2@lid".to_string(), "Will Riker".to_string());
        let inputs = Inputs::default();
        assert_eq!(names.label("1@lid", &inputs), "+17015550101");
        assert_eq!(names.label("2@lid", &inputs), "Will Riker");
        assert_eq!(names.label("3@lid", &inputs), "3@lid");
        assert_eq!(
            names.label("17015550105@s.whatsapp.net", &inputs),
            "+17015550105"
        );
        assert_eq!(names.label("bridge-crew@g.us", &inputs), "bridge-crew@g.us");
    }

    /// `wa.db`'s names: the address book first — over a linked id's own
    /// `lid_display_name` too, since it is what the phone shows — then
    /// `lid_display_name`, then the name someone set themselves, each
    /// found through the linked id's phone number as well as the id.
    #[test]
    fn address_book_then_lid_name_then_own_name() {
        let mut names = JidNames {
            has_contacts: true,
            ..JidNames::default()
        };
        let phone = |n: u8| format!("170155501{n:02}@s.whatsapp.net");
        names.phone_jid.insert("7@lid".into(), phone(3));
        names.phone_jid.insert("9@lid".into(), phone(5));
        names
            .display_name
            .insert("9@lid".into(), "Worf, Son of Mogh".into());
        names.book_name.insert(phone(3), "Data".into());
        names.book_name.insert(phone(5), "Worf".into());
        names.own_name.insert(phone(4), "Geordi La Forge".into());
        names.own_name.insert(phone(3), "Commander Data".into());
        let inputs = Inputs::default();
        assert_eq!(names.label("7@lid", &inputs), "Data", "through the phone");
        assert_eq!(names.label("9@lid", &inputs), "Worf", "book over lid name");
        assert_eq!(names.label(&phone(4), &inputs), "Geordi La Forge");
        assert_eq!(names.label(&phone(6), &inputs), "+17015550106");
        let declared: Vec<String> = inputs
            .declared()
            .into_iter()
            .filter(|i| i.table == "wa_db_contacts")
            .map(|i| i.id)
            .collect();
        assert!(
            declared.contains(&phone(6)),
            "a jid with no contact is still declared, so one added later re-renders: {declared:?}"
        );
    }
}
