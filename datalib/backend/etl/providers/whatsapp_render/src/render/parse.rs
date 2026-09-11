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
use datalib_etl::blob_cas::{self, BlobBundle};
use datalib_etl::periodize::Period;
use datalib_etl_chat_common::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use super::{
    whatsapp_chat_uuid, whatsapp_markdown_uuid, whatsapp_message_uuid, whatsapp_reaction_uuid,
};

/// SQL projection resolving an attachment's `ref_id` — the file's
/// blake3, which render stamps onto each `NormalizedAttachment` — to
/// its CAS entry. Consumed by [`BlobBundle::load`] from the per-chat
/// load below. A row exists only for a file the scan actually saw, so
/// a half-extracted `Media/` tree simply yields no bytes.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT blake3 AS ref_id, blake3,
           mime_type AS content_type,
           relative_path AS upstream_name
      FROM pinned_wa_media_files wa_media_files
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

pub fn parse(raw_dir: &Path, period: Period, source_name: &str) -> Result<ParsedWhatsApp> {
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
            Ok(handle) => handle.block_on(parse_async(&db_path, period, source_name)),
            Err(_) => {
                tokio::runtime::Runtime::new()?.block_on(parse_async(&db_path, period, source_name))
            }
        }
    })
}

async fn parse_async(db_path: &Path, period: Period, source_name: &str) -> Result<ParsedWhatsApp> {
    let pool: SqlitePool = datalib_etl::doltlite_raw::open_reader(db_path)
        .await
        .with_context(|| format!("open {}", db_path.display()))?;

    // Pin before the first read. whatsapp diffs by hand rather than through
    // `scan_buckets`, so it samples HEAD here; no commit means nothing has
    // been committed to render, which is emptiness rather than a reason to
    // read the working set.
    let Some(pin) = datalib_etl::pin::head(&pool).await? else {
        return Ok(ParsedWhatsApp::default());
    };
    datalib_etl::pin::install_views(&pool, &pin)
        .await
        .context("pin the whatsapp raw store for render")?;

    let jids = load_jids(&pool).await?;
    let names = JidNames::load(&pool, &jids).await?;

    // 1) Chats, with their display label. Group chats use `subject`; 1:1
    //    chats fall back to what the JID resolves to.
    let chat_rows =
        sqlx::query("SELECT _id, jid_row_id, subject FROM pinned_chat chat ORDER BY _id")
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
        let subject: Option<String> = r.get("subject");
        let display = subject.clone().unwrap_or_else(|| names.label(&chat_jid));
        chat_idx_by_rowid.insert(rowid, chats.len());
        chats.push(ChatHeader {
            chat_jid,
            display,
            items_by_period: HashMap::new(),
        });
    }

    // 2) Messages. Read once into natural keys; the rowid map is what the
    //    child tables below join through.
    let msg_rows = sqlx::query(
        "SELECT _id, chat_row_id, key_id, from_me, sender_jid_row_id, timestamp, \
                message_type, text_data \
         FROM pinned_message message ORDER BY chat_row_id, sort_id, timestamp, key_id",
    )
    .fetch_all(&pool)
    .await
    .context("select message")?;
    let mut msg_key_by_rowid: HashMap<i64, MsgKey> = HashMap::with_capacity(msg_rows.len());
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
    }

    // 3) Media joined to its `wa_media_files` row to pick up the blake3
    //    the ingest stored alongside the blob_cas put. That blake3 IS the
    //    `blob_refs.ref_id`, so chat-common can stream the bytes out at
    //    render time without render touching disk.
    let media_rows = sqlx::query(
        "SELECT m.message_row_id, m.file_path, m.mime_type, m.file_size, \
                m.media_caption, m.media_name, f.blake3 \
         FROM pinned_message_media m \
         LEFT JOIN pinned_wa_media_files f ON f.relative_path = m.file_path",
    )
    .fetch_all(&pool)
    .await
    .context("select message_media")?;
    let mut media_by_msg: HashMap<MsgKey, Vec<NormalizedAttachment>> = HashMap::new();
    let mut unresolved: Vec<String> = Vec::new();
    for r in &media_rows {
        let Some(key) = msg_key_by_rowid.get(&r.get::<i64, _>("message_row_id")) else {
            continue;
        };
        let file_path: Option<String> = r.get("file_path");
        let media_name: Option<String> = r.get("media_name");
        let mime_type: Option<String> = r.get("mime_type");
        let file_size: Option<i64> = r.get("file_size");
        let media_caption: Option<String> = r.get("media_caption");
        let ref_id: Option<String> = r.get("blake3");
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
            examples = ?unresolved.iter().take(3).collect::<Vec<_>>(),
            "message_media.file_path matched no wa_media_files row; \
             those attachments render as placeholders",
        );
    }

    // 4) Reactions: an add-on row keyed like a message, plus its emoji.
    let react_rows = sqlx::query(
        "SELECT a.chat_row_id, a.key_id, a.from_me, a.sender_jid_row_id, \
                a.parent_message_row_id, a.timestamp, r.reaction \
         FROM pinned_message_add_on a \
         JOIN pinned_message_add_on_reaction r ON r.message_add_on_row_id = a._id \
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
        let Some(chat_jid) = chat_row_id
            .and_then(|i| chat_idx_by_rowid.get(&i))
            .map(|&i| chats[i].chat_jid.as_str())
        else {
            tracing::warn!("message_add_on: chat_row_id not in chat; dropping reaction");
            continue;
        };
        let key_id: String = r.get("key_id");
        let from_me: i64 = r.get::<Option<i64>, _>("from_me").unwrap_or(0);
        let sender_jid = r
            .get::<Option<i64>, _>("sender_jid_row_id")
            .and_then(|i| jids.get(&i));
        let emoji: Option<String> = r.get("reaction");
        let timestamp: Option<i64> = r.get("timestamp");
        let reactor_display = match sender_jid {
            Some(j) => names.label(j),
            None if from_me == 1 => "Me".to_string(),
            None => "?".to_string(),
        };
        reactions_by_parent
            .entry(parent.clone())
            .or_default()
            .push(NormalizedReaction {
                reaction_uuid: whatsapp_reaction_uuid(source_name, chat_jid, &key_id, from_me),
                reactor_display,
                source_ref: None,
                emoji: emoji.unwrap_or_else(|| "?".to_string()),
                // A NULL `timestamp` column is "we don't know when",
                // which is a null `when_ts` — not 1970.
                date_ms: timestamp,
            });
    }

    // 5) Walk messages, bucket by period, attach media + reactions.
    for r in &msg_rows {
        let Some(key) = msg_key_by_rowid.get(&r.get::<i64, _>("_id")) else {
            continue;
        };
        let idx = chat_idx_by_rowid[&r.get::<i64, _>("chat_row_id")];
        let sender_jid = r
            .get::<Option<i64>, _>("sender_jid_row_id")
            .and_then(|i| jids.get(&i).cloned());
        let item = build_item(
            source_name,
            key,
            sender_jid,
            r,
            &names,
            media_by_msg.remove(key).unwrap_or_default(),
            reactions_by_parent.remove(key).unwrap_or_default(),
        );
        let period_key = match r.get::<Option<i64>, _>("timestamp") {
            Some(ts) => period.key_for_ms(ts),
            // An undated message still has to be filed somewhere;
            // `key_for_undated` documents why that is the epoch
            // bucket and not a new `"undated"` key. Its `when_ts`
            // is null regardless — bucketing and `when_ts` answer
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
        let chat_uuid = whatsapp_chat_uuid(source_name, &ch.chat_jid);
        let mut keys: Vec<String> = ch.items_by_period.keys().cloned().collect();
        keys.sort();
        let mut buckets: Vec<NormalizedDoc> = Vec::with_capacity(keys.len());
        let mut items_by_period = ch.items_by_period;
        for k in keys {
            let mut items = items_by_period.remove(&k).unwrap_or_default();
            items.sort_by_key(|i| i.date_ms);
            buckets.push(NormalizedDoc {
                orphan_reactions: Vec::new(),
                period_key: k.clone(),
                markdown_uuid: whatsapp_markdown_uuid(&chat_uuid, &k),
                items,
            });
        }
        out.push(NormalizedChat {
            path_prefix: None,
            id: ch.chat_jid.clone(),
            chat_uuid,
            display: ch.display,
            author: None,
            account: None,
            project: None,
            external_id: Some(ch.chat_jid),
            source_url: None,
            upstream_scope: None,
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
    let cas_path = blob_cas::cas_path_for(db_path);
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    if cas_path.is_file() {
        let cas_pool: SqlitePool = datalib_etl::doltlite_raw::open_reader(&cas_path)
            .await
            .with_context(|| format!("open CAS for render at {}", cas_path.display()))?;
        for chat in &out {
            let mut seen: HashSet<String> = HashSet::new();
            let mut refs: Vec<String> = Vec::new();
            for bucket in &chat.buckets {
                for item in &bucket.items {
                    for att in &item.attachments {
                        if let Some(r) = att.ref_id.as_deref() {
                            if seen.insert(r.to_string()) {
                                refs.push(r.to_string());
                            }
                        }
                    }
                }
            }
            if refs.is_empty() {
                continue;
            }
            let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
            let bundle =
                BlobBundle::load(&pool, &cas_pool, ATTACHMENTS_PROJECTION_SQL, &ref_strs).await?;
            if !bundle.is_empty() {
                blobs_by_chat.insert(chat.id.clone(), bundle);
            }
        }
        cas_pool.close().await;
    }
    pool.close().await;

    Ok(ParsedWhatsApp {
        chats: out,
        blobs_by_chat,
    })
}

fn build_item(
    source_name: &str,
    key: &MsgKey,
    sender_jid: Option<String>,
    r: &sqlx::sqlite::SqliteRow,
    names: &JidNames,
    attachments: Vec<NormalizedAttachment>,
    reactions: Vec<NormalizedReaction>,
) -> NormalizedChatItem {
    let timestamp: Option<i64> = r.get("timestamp");
    let message_type: Option<i64> = r.get("message_type");
    let text_data: Option<String> = r.get("text_data");

    let author_display = if key.from_me == 1 {
        "Me".to_string()
    } else if let Some(j) = sender_jid.as_deref() {
        names.label(j)
    } else {
        // 1:1 incoming: the chat JID IS the sender, by definition.
        names.label(&key.chat_jid)
    };
    let author_id = sender_jid.unwrap_or_else(|| format!("chat:{}", key.chat_jid));

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

    NormalizedChatItem {
        message_uuid: whatsapp_message_uuid(source_name, &key.chat_jid, &key.key_id, key.from_me),
        author_id,
        author_display,
        // A NULL `timestamp` column is "we don't know when", which is a
        // null `when_ts` — not 1970. See
        // `docs/dev/data_architecture_parse_and_render.md` §6.
        date_ms: timestamp,
        text: text_data,
        kind,
        attachments,
        reactions,
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
    }
}

struct ChatHeader {
    chat_jid: String,
    display: String,
    items_by_period: HashMap<String, Vec<NormalizedChatItem>>,
}

/// `jid._id -> raw_string`. A few seed rows carry a NULL `raw_string`;
/// `user@server` is spelled for those so a row still has a key.
async fn load_jids(pool: &SqlitePool) -> Result<HashMap<i64, String>> {
    let rows = sqlx::query(
        "SELECT _id, coalesce(raw_string, user || '@' || server) AS jid FROM pinned_jid jid",
    )
    .fetch_all(pool)
    .await
    .context("select jid")?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<i64, _>("_id"), r.get::<String, _>("jid")))
        .collect())
}

/// Older msgstore versions have neither of the two LID tables. Absent is
/// "nothing to map", not a failed render.
async fn has_table(pool: &SqlitePool, table: &str) -> Result<bool> {
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_one(pool)
            .await
            .with_context(|| format!("probe for {table}"))?;
    Ok(n > 0)
}

/// What msgstore knows about who a JID is. A `…@lid` (linked id) is an
/// opaque number; `lid_display_name` may name it and `jid_map` may say
/// which phone number it stands for. Neither table is total — the
/// measured backup mapped 5 of 6 `@lid` chats and named none — so the
/// raw JID stays as the last resort, and `label_from_jid` is what makes
/// a phone-number JID dialable.
#[derive(Default)]
struct JidNames {
    display_name: HashMap<String, String>,
    phone_jid: HashMap<String, String>,
}

impl JidNames {
    async fn load(pool: &SqlitePool, jids: &HashMap<i64, String>) -> Result<Self> {
        let mut out = Self::default();
        if has_table(pool, "lid_display_name").await? {
            let rows = sqlx::query(
                "SELECT lid_row_id, display_name FROM pinned_lid_display_name lid_display_name",
            )
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
        if has_table(pool, "jid_map").await? {
            let rows = sqlx::query("SELECT lid_row_id, jid_row_id FROM pinned_jid_map jid_map")
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

    fn label(&self, jid: &str) -> String {
        if let Some(name) = self.display_name.get(jid) {
            return name.clone();
        }
        let jid = self.phone_jid.get(jid).map(String::as_str).unwrap_or(jid);
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
    use super::JidNames;

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
        assert_eq!(names.label("1@lid"), "+17015550101");
        assert_eq!(names.label("2@lid"), "Will Riker");
        assert_eq!(names.label("3@lid"), "3@lid");
        assert_eq!(names.label("17015550105@s.whatsapp.net"), "+17015550105");
        assert_eq!(names.label("bridge-crew@g.us"), "bridge-crew@g.us");
    }
}
