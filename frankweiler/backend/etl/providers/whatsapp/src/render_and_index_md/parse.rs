//! Read the `wa_*` tables out of the raw doltlite store and assemble
//! `Vec<NormalizedChat>` for chat-common's renderer.
//!
//! Pulls all rows up-front (the raw stores in scope here are tens of
//! MB at most) so the renderer can walk in memory without re-querying.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobBundle};
use frankweiler_etl::periodize::Period;
use frankweiler_etl_chat_common::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use super::{
    whatsapp_chat_uuid, whatsapp_markdown_uuid, whatsapp_message_uuid, whatsapp_reaction_uuid,
};

/// SQL projection that maps an attachment's `sha256` (the upstream
/// `ref_id` translate stamps onto each `NormalizedAttachment`) to its
/// CAS `blake3`. Consumed by [`BlobBundle::load`] from the per-chat
/// load below. Returns one row per requested ref_id whose bytes are
/// known to the CAS — `blake3 IS NOT NULL` guards against
/// half-extracted Media/ trees.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT sha256 AS ref_id, blake3,
           mime_type AS content_type,
           relative_path AS upstream_name
      FROM wa_media_files
     WHERE sha256 IN ({placeholders}) AND blake3 IS NOT NULL";

/// What `parse` returns to render: the chat tree plus a per-chat
/// `BlobBundle` (keyed by `NormalizedChat::id`) carrying every
/// attachment's bytes pre-loaded from the sibling CAS. Mirrors slack's
/// `(ParsedSlack { threads: SlackThreadBucket{ blobs }, … })` shape —
/// the bundle is the synchronous bag the chat-common renderer reads
/// at `materialize_to_dir` time.
#[derive(Default)]
pub struct ParsedWhatsApp {
    pub chats: Vec<NormalizedChat>,
    pub blobs_by_chat: HashMap<String, BlobBundle>,
}

/// Open the raw store and build the normalized chat tree.
///
/// `raw_dir` is the source's `input_path` (sync sets it to
/// `<data_root>/whatsapp/raw/`); the doltlite file is found via
/// `doltlite_raw::db_path_for`. `period` controls how items are
/// bucketed into rendered .md files.
/// `source_name` is the YAML source name; goes into every UUID seed
/// so two YAML sources backed by different phones don't collide.
pub fn parse(raw_dir: &Path, period: Period, source_name: &str) -> Result<ParsedWhatsApp> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(ParsedWhatsApp::default());
    }
    // Bridge to sync sqlx code: this fn is called from a sync context
    // (translate phase). We need to spin up a tokio runtime since sqlx
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
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        .read_only(true);
    let pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", db_path.display()))?;

    // 1) Pull every chat with its display label. Group chats use
    //    `subject`; 1:1 chats fall back to the JID's local part.
    let chat_rows =
        sqlx::query("SELECT chat_jid, subject, group_type FROM wa_chat ORDER BY chat_jid")
            .fetch_all(&pool)
            .await
            .context("select wa_chat")?;
    let mut chats: Vec<ChatHeader> = chat_rows
        .iter()
        .map(|r| {
            let chat_jid: String = r.get("chat_jid");
            let subject: Option<String> = r.get("subject");
            let group_type: Option<i64> = r.get("group_type");
            let display = subject.clone().unwrap_or_else(|| label_from_jid(&chat_jid));
            ChatHeader {
                chat_jid,
                display,
                is_group: group_type.unwrap_or(0) > 0,
                items_by_period: HashMap::new(),
                _subject_kept_for_search: subject,
            }
        })
        .collect();
    let mut chats_idx: HashMap<String, usize> = chats
        .iter()
        .enumerate()
        .map(|(i, c)| (c.chat_jid.clone(), i))
        .collect();

    // 2) Messages.
    let msg_rows = sqlx::query(
        "SELECT chat_jid, key_id, from_me, sender_jid, timestamp, message_type, text_data \
         FROM wa_message ORDER BY chat_jid, sort_id, timestamp, key_id",
    )
    .fetch_all(&pool)
    .await
    .context("select wa_message")?;

    // 3) Media joined to its `wa_media_files` row to pick up the
    //    `sha256` extract stored alongside the blob_cas put. That sha256
    //    IS the `blob_refs.ref_id`, so chat-common can stream the bytes
    //    out at render time without translate ever touching disk.
    let media_rows = sqlx::query(
        "SELECT m.chat_jid, m.key_id, m.from_me, m.file_path, m.mime_type, m.file_size, \
                m.media_caption, m.media_name, f.sha256 \
         FROM wa_message_media m \
         LEFT JOIN wa_media_files f ON f.relative_path = m.file_path",
    )
    .fetch_all(&pool)
    .await
    .context("select wa_message_media")?;
    let mut media_by_msg: HashMap<(String, String, i64), Vec<NormalizedAttachment>> =
        HashMap::new();
    for r in &media_rows {
        let key = (
            r.get::<String, _>("chat_jid"),
            r.get::<String, _>("key_id"),
            r.get::<i64, _>("from_me"),
        );
        let file_path: Option<String> = r.get("file_path");
        let media_name: Option<String> = r.get("media_name");
        let mime_type: Option<String> = r.get("mime_type");
        let file_size: Option<i64> = r.get("file_size");
        let media_caption: Option<String> = r.get("media_caption");
        // `sha256` is the `blob_refs.ref_id` extract stored at put time
        // (see `extract::mirror_media_files`). `None` here means either
        // the file went missing between scan and put, or the message's
        // `file_path` didn't resolve to a row in `wa_media_files`
        // (corrupted backup, partial Media/ tree, …). In either case the
        // renderer's "(not yet fetched)" placeholder fires.
        media_by_msg
            .entry(key)
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
                ref_id: r.get("sha256"),
            });
    }

    // 4) Reactions: addon row + its reaction emoji.
    let react_rows = sqlx::query(
        "SELECT a.chat_jid AS add_on_chat_jid, a.key_id AS add_on_key_id, a.from_me AS add_on_from_me, \
                a.sender_jid, a.parent_chat_jid, a.parent_key_id, a.parent_from_me, \
                a.timestamp, r.reaction \
         FROM wa_message_add_on a \
         JOIN wa_message_add_on_reaction r \
            ON r.chat_jid = a.chat_jid AND r.key_id = a.key_id AND r.from_me = a.from_me \
         WHERE a.parent_key_id IS NOT NULL",
    )
    .fetch_all(&pool)
    .await
    .context("select wa_message_add_on")?;
    let mut reactions_by_parent: HashMap<(String, String, i64), Vec<NormalizedReaction>> =
        HashMap::new();
    for r in &react_rows {
        let parent_key = (
            r.get::<String, _>("parent_chat_jid"),
            r.get::<String, _>("parent_key_id"),
            r.get::<i64, _>("parent_from_me"),
        );
        let add_on_chat_jid: String = r.get("add_on_chat_jid");
        let add_on_key_id: String = r.get("add_on_key_id");
        let add_on_from_me: i64 = r.get("add_on_from_me");
        let sender_jid: Option<String> = r.get("sender_jid");
        let emoji: Option<String> = r.get("reaction");
        let timestamp: Option<i64> = r.get("timestamp");
        let reactor_display = match sender_jid.as_deref() {
            Some(j) => label_from_jid(j),
            None if add_on_from_me == 1 => "Me".to_string(),
            None => "?".to_string(),
        };
        reactions_by_parent
            .entry(parent_key)
            .or_default()
            .push(NormalizedReaction {
                reaction_uuid: whatsapp_reaction_uuid(
                    source_name,
                    &add_on_chat_jid,
                    &add_on_key_id,
                    add_on_from_me,
                ),
                reactor_display,
                emoji: emoji.unwrap_or_else(|| "?".to_string()),
                date_ms: timestamp.unwrap_or(0),
            });
    }

    // 5) Walk messages, bucket by period, attach media + reactions.
    for r in &msg_rows {
        let chat_jid: String = r.get("chat_jid");
        let Some(&idx) = chats_idx.get(&chat_jid) else {
            // Orphan message — chat row missing. Synthesize a
            // placeholder chat so the message still surfaces; downstream
            // search will show it under a `(unknown chat)` heading.
            let display = label_from_jid(&chat_jid);
            let header = ChatHeader {
                chat_jid: chat_jid.clone(),
                display: display.clone(),
                is_group: false,
                items_by_period: HashMap::new(),
                _subject_kept_for_search: None,
            };
            chats.push(header);
            let new_idx = chats.len() - 1;
            chats_idx.insert(chat_jid.clone(), new_idx);
            tracing::warn!(
                chat_jid,
                "wa_message without wa_chat — synthesized placeholder"
            );
            let _ = display;
            // fall through using new_idx below
            let key_id: String = r.get("key_id");
            let from_me: i64 = r.get("from_me");
            let item = build_item(
                source_name,
                &chat_jid,
                &key_id,
                from_me,
                r,
                &mut media_by_msg,
                &mut reactions_by_parent,
            );
            let ts = r.get::<Option<i64>, _>("timestamp").unwrap_or(0);
            let period_key = period.key_for_ms(ts);
            chats[new_idx]
                .items_by_period
                .entry(period_key)
                .or_default()
                .push(item);
            continue;
        };
        let key_id: String = r.get("key_id");
        let from_me: i64 = r.get("from_me");
        let item = build_item(
            source_name,
            &chat_jid,
            &key_id,
            from_me,
            r,
            &mut media_by_msg,
            &mut reactions_by_parent,
        );
        let ts = r.get::<Option<i64>, _>("timestamp").unwrap_or(0);
        let period_key = period.key_for_ms(ts);
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
                period_key: k.clone(),
                markdown_uuid: whatsapp_markdown_uuid(&chat_uuid, &k),
                items,
            });
        }
        out.push(NormalizedChat {
            id: ch.chat_jid.clone(),
            chat_uuid,
            display: ch.display,
            account: None,
            project: None,
            external_id: Some(ch.chat_jid),
            source_url: None,
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
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))
            .with_context(|| format!("sqlite uri for {}", cas_path.display()))?
            .read_only(true);
        let cas_pool: SqlitePool = SqlitePoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(60))
            .connect_with(cas_opts)
            .await
            .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?;
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

#[allow(clippy::too_many_arguments)]
fn build_item(
    source_name: &str,
    chat_jid: &str,
    key_id: &str,
    from_me: i64,
    r: &sqlx::sqlite::SqliteRow,
    media_by_msg: &mut HashMap<(String, String, i64), Vec<NormalizedAttachment>>,
    reactions_by_parent: &mut HashMap<(String, String, i64), Vec<NormalizedReaction>>,
) -> NormalizedChatItem {
    let sender_jid: Option<String> = r.get("sender_jid");
    let timestamp: Option<i64> = r.get("timestamp");
    let message_type: Option<i64> = r.get("message_type");
    let text_data: Option<String> = r.get("text_data");

    let author_display = if from_me == 1 {
        "Me".to_string()
    } else if let Some(j) = sender_jid.clone() {
        label_from_jid(&j)
    } else {
        // 1:1 incoming: the chat JID IS the sender, by definition.
        label_from_jid(chat_jid)
    };
    let author_id = sender_jid
        .clone()
        .unwrap_or_else(|| format!("chat:{chat_jid}"));

    let key = (chat_jid.to_string(), key_id.to_string(), from_me);
    let attachments = media_by_msg.remove(&key).unwrap_or_default();
    let reactions = reactions_by_parent.remove(&key).unwrap_or_default();

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
        message_uuid: whatsapp_message_uuid(source_name, chat_jid, key_id, from_me),
        author_id,
        author_display,
        date_ms: timestamp.unwrap_or(0),
        text: text_data,
        kind,
        attachments,
        reactions,
        system_note: None,
        source_url: None,
        kind_label: None,
    }
}

struct ChatHeader {
    chat_jid: String,
    display: String,
    #[allow(dead_code)]
    is_group: bool,
    items_by_period: HashMap<String, Vec<NormalizedChatItem>>,
    /// Subject as it appeared on the chat row; kept so a future
    /// translate revision can include it in grid_row text content
    /// without re-reading the source DB.
    #[allow(dead_code)]
    _subject_kept_for_search: Option<String>,
}

/// Pull a short human label out of a JID. "17015550101@s.whatsapp.net"
/// → "+17015550101"; "bridge-crew@g.us" → "bridge-crew@g.us" (kept
/// verbatim so the group's stable id is visible to the reader).
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
