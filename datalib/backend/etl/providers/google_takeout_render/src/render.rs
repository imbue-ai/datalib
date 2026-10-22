//! Render the Takeout feeds into markdown via the shared chat renderer:
//! Google Chat spaces and Google Voice conversations a document per
//! month, the activity feeds (Gemini, YouTube, Google Maps — see
//! [`crate::feeds`]) a document per year.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::changed_chats;
use datalib_etl_chat_common::render::{Buckets, ChatRenderer, RenderProfile};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, UpstreamRef,
};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_chat_common::{render_batched, ChatBlobs};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::escape_md_block;
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_handle::Handle;
use datalib_schema::problems::Problem;
use serde_json::Value;

use crate::feeds::{self, Row};
use crate::{chat, gemini, ids, maps, pages, youtube};

use datalib_etl_google_takeout::ingest::google_voice::schema_raw::VoiceAttachmentRow;
use datalib_etl_google_takeout::ingest::{db_path_for, RawDb};
use datalib_schema::providers::Provider;

/// v2: a `created_date` / `when` we cannot parse gets a null `created_at`
///     instead of a real-looking `1970-01-01T00:00:00`. See
///     `docs/dev/data_architecture_parse_and_render.md` §6.
/// v3: ids are minted through `datalib_id`, every row carries its
///     backpointer, and a message's id carries its stamp in its leading
///     bits (`datalib_id`'s v8 layout). Every uuid moved, `chat_uuid`
///     among them.
/// v5: the author span carries the author's handle as `data-handle`.
/// v6: a `+1` number without ten digits after the 1 has no handle.
/// v7: Gemini, YouTube and Google Maps render, a document per year.
/// v9: Maps reviews draw their follow-up answers, and Google's
///     no-location sentence is a note; Saved lists, My Maps maps and
///     Play Books books render, a page each.
/// v8: a Chat message draws its attachments, earlier versions, quote,
///     thread, previews, reactions and labels, and a space its name; a
///     problem row for every field render leaves unread.
pub const RENDER_VERSION: u32 = 9;

/// Projection for `BlobBundle::load_many` over the Voice CAS edge: the
/// `ref_name` (attachment filename) is the bundle key; `content_type`
/// falls back to `cas_objects` (we don't store it on the edge).
const VOICE_BLOB_PROJECTION: &str = "SELECT ref_name AS ref_id, blake3, \
            NULL AS content_type, NULL AS upstream_name \
     FROM voice_attachments \
     WHERE ref_name IN ({placeholders}) AND blake3 IS NOT NULL";

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::GoogleTakeout,
        source_label: "Google Chat".to_string(),
        chat_kind: "Google Chat".to_string(),
        message_kind: "Google Chat Message".to_string(),
        reaction_kind: "Google Chat Reaction".to_string(),
        chat_entity_kind: ids::KIND_SPACE,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

fn voice_profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::GoogleTakeout,
        source_label: "Google Voice".to_string(),
        chat_kind: "Google Voice Conversation".to_string(),
        message_kind: "Google Voice Message".to_string(),
        reaction_kind: "Google Voice Reaction".to_string(),
        chat_entity_kind: ids::KIND_VOICE_CONVERSATION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

/// A Chat attachment edge's row id is the bundle key.
const CHAT_BLOB_PROJECTION: &str = "SELECT id AS ref_id, blake3, \
            NULL AS content_type, export_name AS upstream_name \
     FROM chat_attachments \
     WHERE id IN ({placeholders}) AND blake3 IS NOT NULL";

/// The Gemini attachment edge's row id is the bundle key.
const GEMINI_BLOB_PROJECTION: &str = "SELECT id AS ref_id, blake3, \
            NULL AS content_type, filename AS upstream_name \
     FROM gemini_attachments \
     WHERE id IN ({placeholders}) AND blake3 IS NOT NULL";

/// A Maps photo row holds its own bytes' key; its id is the bundle key.
const MAPS_PHOTO_BLOB_PROJECTION: &str = "SELECT id AS ref_id, blake3, \
            NULL AS content_type, id AS upstream_name \
     FROM maps_photos \
     WHERE id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Every row one pass reads off the store. Attachment bytes are not
/// among them: render reads those a batch of chats at a time.
struct Loaded {
    messages: Vec<(String, Value)>,
    groups: Vec<(String, Value)>,
    users: Vec<(String, Value)>,
    voice_messages: Vec<(String, Value)>,
    gemini: Vec<Row>,
    watches: Vec<Row>,
    subscriptions: Vec<Row>,
    reviews: Vec<Row>,
    saved: Vec<Row>,
    photos: Vec<Row>,
    saved_lists: Vec<pages::FileRow>,
    placemarks: Vec<pages::FileRow>,
    book_entries: Vec<pages::FileRow>,
    scan: datalib_etl::doltlite_raw::DiffScan,
}

/// What one render pass did: the buckets to declare and the commit read.
#[derive(Debug, Default)]
pub struct RenderOutcome {
    /// Every conversation this run looked at, named first with nothing
    /// and then, for the rendered ones, with what they read.
    pub buckets: Buckets,
    pub new_head: Option<String>,
}

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
    // Pinned at open: the driver's pin when it made one, else HEAD. No
    // commit means nothing has been committed here to render, which is
    // emptiness rather than a reason to read the working set.
    let Some(db) = block_on(RawDb::open_reader(&db_path, range.pin))? else {
        return Ok(RenderOutcome::default());
    };
    let rendered = render_open(
        &db,
        out_root,
        source_id,
        progress,
        on_doc_complete,
        range,
        blob_bytes,
    );
    // Closed, not dropped: the next open of this store is a second
    // connection until this one is actually gone.
    block_on(db.close());
    rendered
}

fn render_open(
    db: &RawDb,
    out_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    range: RawRange<'_>,
    blob_bytes: u64,
) -> Result<RenderOutcome> {
    let pin = db.pin().expect("a reader is pinned at open").clone();
    let (all_chats, scan) = {
        let l = block_on(load(db, range.cursor, &pin))?;
        let mut all_chats = chat::build(source_id, &l.messages, &l.groups, &l.users);
        all_chats.extend(build_voice_chats(source_id, &l.voice_messages));
        all_chats.extend(gemini::build(source_id, &l.gemini));
        all_chats.extend(youtube::build_history(source_id, &l.watches));
        all_chats.extend(youtube::build_subscriptions(source_id, &l.subscriptions));
        all_chats.extend(maps::build(source_id, &l.reviews, &l.saved, &l.photos));
        all_chats.extend(pages::saved_lists(source_id, &l.saved_lists));
        all_chats.extend(pages::my_maps(source_id, &l.placemarks));
        all_chats.extend(pages::play_books(source_id, &l.book_entries));
        (all_chats, l.scan)
    };
    let changed = changed_chats(all_chats, range, scan.render.as_ref(), |id| {
        chat_uuid_for(source_id, id)
    });
    let mut outcome = RenderOutcome {
        buckets: changed.buckets,
        new_head: scan.new_head,
    };
    let (feed_chats, chats): (Vec<NormalizedChat>, Vec<NormalizedChat>) = changed
        .chats
        .into_iter()
        .partition(|c| feeds::is_feed(&c.id));
    let (voice_chats, chats): (Vec<NormalizedChat>, Vec<NormalizedChat>) =
        chats.into_iter().partition(|c| is_voice(&c.id));

    let blobs = ChatBlobs::new(db.pool(), Some(db.cas()), &blob_projection);
    let mut render_as = |profile: &RenderProfile, chats: &[NormalizedChat]| -> Result<()> {
        if chats.is_empty() {
            return Ok(());
        }
        let mut renderer = ChatRenderer::new(
            profile,
            out_root,
            source_id,
            chats.iter().map(|c| c.buckets.len()).sum(),
            progress,
            &mut *on_doc_complete,
        );
        render_batched(&mut renderer, chats, &blobs, blob_bytes)?;
        outcome.buckets.extend(renderer.finish().buckets);
        Ok(())
    };
    render_as(&profile(), &chats)?;
    render_as(&voice_profile(), &voice_chats)?;
    for feed in feed_chats {
        render_as(&feeds::profile(&feed.id), std::slice::from_ref(&feed))?;
    }
    Ok(outcome)
}

/// The edge table a chat's attachments are named in.
fn blob_projection(chat: &NormalizedChat) -> &'static str {
    match chat.id.as_str() {
        feeds::GEMINI => GEMINI_BLOB_PROJECTION,
        feeds::MAPS => MAPS_PHOTO_BLOB_PROJECTION,
        id if is_voice(id) => VOICE_BLOB_PROJECTION,
        _ => CHAT_BLOB_PROJECTION,
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

async fn load(db: &RawDb, cursor: Option<&str>, pin: &datalib_etl::pin::Pin) -> Result<Loaded> {
    Ok(Loaded {
        messages: db.load_payloads_with_id("chat_messages").await?,
        // (dir name, group_info payload) — the directory name carries the
        // space id, which `group_info.json` itself does not.
        groups: db.load_payloads_with_id("chat_groups").await?,
        users: db.load_payloads_with_id("chat_users").await?,
        voice_messages: db.load_payloads_with_id("voice_messages").await?,
        gemini: load_rows(db, "gemini_activity", Dated::Yes).await?,
        watches: load_rows(db, "youtube_watch_history", Dated::Yes).await?,
        subscriptions: load_rows(db, "youtube_subscriptions", Dated::No).await?,
        reviews: load_rows(db, "maps_reviews", Dated::Yes).await?,
        saved: load_rows(db, "maps_saved_places", Dated::Yes).await?,
        photos: load_rows(db, "maps_photos", Dated::Yes).await?,
        saved_lists: load_file_rows(db, "maps_saved_lists", Dated::No).await?,
        placemarks: load_file_rows(db, "my_maps_placemarks", Dated::No).await?,
        book_entries: load_file_rows(db, "play_books_annotations", Dated::Yes).await?,
        scan: scan_diff(db.pool(), cursor, pin).await?,
    })
}

enum Dated {
    Yes,
    No,
}

async fn load_rows(db: &RawDb, table: &'static str, dated: Dated) -> Result<Vec<Row>> {
    let when = match dated {
        Dated::Yes => "when_ts",
        Dated::No => "NULL",
    };
    // Audited: `table` is a literal at every call, `when` one of two.
    let sql = format!("SELECT id, json(payload), {when} FROM {table} ORDER BY id");
    let rows: Vec<(String, Option<String>, Option<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_all(db.pool())
            .await
            .with_context(|| format!("select {table}"))?;
    rows.into_iter()
        .map(|(id, payload, when)| {
            let payload = match payload {
                Some(p) => serde_json::from_str(&p).with_context(|| format!("{table} {id}"))?,
                None => Value::Null,
            };
            Ok(Row { id, payload, when })
        })
        .collect()
}

/// A table whose rows each name the export file they came from.
async fn load_file_rows(
    db: &RawDb,
    table: &'static str,
    dated: Dated,
) -> Result<Vec<pages::FileRow>> {
    let when = match dated {
        Dated::Yes => "when_ts",
        Dated::No => "NULL",
    };
    // Audited: `table` is a literal at every call, `when` one of two.
    let sql = format!("SELECT id, json(payload), {when}, file FROM {table} ORDER BY id");
    let rows: Vec<(String, Option<String>, Option<String>, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .fetch_all(db.pool())
            .await
            .with_context(|| format!("select {table}"))?;
    rows.into_iter()
        .map(|(id, payload, when, file)| {
            let payload = match payload {
                Some(p) => serde_json::from_str(&p).with_context(|| format!("{table} {id}"))?,
                None => Value::Null,
            };
            Ok(pages::FileRow {
                row: Row { id, payload, when },
                file,
            })
        })
        .collect()
}

/// The chat uuid a conversation id mints to, for a conversation the diff
/// named that no longer has a message: a Google Chat space, a Google
/// Voice conversation carrying its `voice:` prefix, or an activity feed
/// carrying its `feed:` one.
fn chat_uuid_for(source_id: &str, id: &str) -> String {
    if feeds::is_feed(id) {
        ids::feed(source_id, id).uuid
    } else if is_voice(id) {
        ids::voice_conversation(source_id, id).uuid
    } else {
        ids::space(source_id, id).uuid
    }
}

fn is_voice(chat_id: &str) -> bool {
    chat_id.starts_with("voice:")
}

/// Which conversations a new or changed row maps to: a Google Chat
/// message names its space (the first segment of its id), a group's
/// members the spaces of the messages filed under it, a Google Voice
/// message or attachment its conversation, an activity row its feed. Everything a rendered
/// conversation read is declared, so this only has to catch what the
/// declarations cannot — rows that were not there to declare.
async fn scan_diff(
    pool: &sqlx::SqlitePool,
    cursor: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<datalib_etl::doltlite_raw::DiffScan> {
    datalib_etl::doltlite_raw::scan_buckets(
        pool,
        cursor,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT bucket FROM (
                    SELECT CASE WHEN instr(m.id, '/') > 0
                                THEN substr(m.id, 1, instr(m.id, '/') - 1)
                                ELSE m.id END AS bucket
                      FROM (SELECT coalesce(to_id, from_id) AS id
                              FROM dolt_diff_chat_messages
                             WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged') m
                    UNION
                    SELECT CASE WHEN instr(m.id, '/') > 0
                                THEN substr(m.id, 1, instr(m.id, '/') - 1)
                                ELSE m.id END
                      FROM dolt_diff_chat_groups g
                      JOIN chat_messages m ON m.group_id = coalesce(g.to_id, g.from_id)
                     WHERE g.from_ref = ?1 AND g.to_ref = ?2 AND g.diff_type != 'unchanged'
                    UNION
                    SELECT CASE WHEN instr(m.id, '/') > 0
                                THEN substr(m.id, 1, instr(m.id, '/') - 1)
                                ELSE m.id END
                      FROM dolt_diff_chat_attachments a
                      JOIN chat_messages m ON m.id = coalesce(a.to_message_id, a.from_message_id)
                     WHERE a.from_ref = ?1 AND a.to_ref = ?2 AND a.diff_type != 'unchanged'
                    UNION
                    SELECT CASE WHEN instr(m.id, '/') > 0
                                THEN substr(m.id, 1, instr(m.id, '/') - 1)
                                ELSE m.id END
                      FROM chat_messages m
                     WHERE EXISTS (SELECT 1 FROM dolt_diff_chat_users
                                    WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged')
                    UNION
                    SELECT 'voice:' || coalesce(to_conversation_key, from_conversation_key)
                      FROM dolt_diff_voice_messages
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'voice:' || m.conversation_key
                      FROM dolt_diff_voice_attachments d
                      JOIN voice_messages m ON m.id = coalesce(d.to_message_id, d.from_message_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:gemini' FROM dolt_diff_gemini_activity
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:gemini' FROM dolt_diff_gemini_attachments
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:youtube_history' FROM dolt_diff_youtube_watch_history
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:youtube_subscriptions' FROM dolt_diff_youtube_subscriptions
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:maps' FROM dolt_diff_maps_reviews
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:maps' FROM dolt_diff_maps_saved_places
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:maps' FROM dolt_diff_maps_photos
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:saved_list:' || coalesce(to_file, from_file)
                      FROM dolt_diff_maps_saved_lists
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:my_map:' || coalesce(to_file, from_file)
                      FROM dolt_diff_my_maps_placemarks
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT 'feed:play_book:' || coalesce(to_file, from_file)
                      FROM dolt_diff_play_books_annotations
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                )
                WHERE bucket IS NOT NULL AND bucket != '' AND bucket != 'voice:'
            ",
        },
    )
    .await
}

// ── Google Voice ────────────────────────────────────────────────────

fn voice_chat_id(m: &Value) -> String {
    let key = m
        .get("conversation_key")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    format!("voice:{key}")
}

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
fn build_voice_chats(source_id: &str, messages: &[(String, Value)]) -> Vec<NormalizedChat> {
    let mut by_chat: BTreeMap<String, (Vec<&Value>, Inputs)> = BTreeMap::new();
    for (row_id, m) in messages {
        let (rows, inputs) = by_chat.entry(voice_chat_id(m)).or_default();
        inputs.read("voice_messages", row_id);
        for ref_name in voice_attachment_refs(m) {
            inputs.read(
                "voice_attachments",
                &VoiceAttachmentRow::pk_recipe(row_id, &ref_name),
            );
        }
        rows.push(m);
    }

    let mut chats = Vec::with_capacity(by_chat.len());
    for (chat_id, (msgs, inputs)) in by_chat {
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
        let mut items: Vec<NormalizedChatItem> =
            msgs.iter().map(|m| voice_item(source_id, m)).collect();
        items.sort_by_key(|i| i.date_ms);

        // Month buckets (`YYYY-MM`), oldest first.
        let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
        for it in items {
            by_month.entry(month_of(it.date_ms)).or_default().push(it);
        }
        let buckets: Vec<NormalizedDoc> = by_month
            .into_iter()
            .map(|(period_key, items)| {
                let month = ids::voice_month(source_id, &chat_id, &period_key);
                NormalizedDoc {
                    orphan_reactions: Vec::new(),
                    markdown_uuid: month.uuid,
                    source_ref: Some(UpstreamRef::new(month.entity_kind, month.natural_key)),
                    period_key,
                    items,
                }
            })
            .collect();

        let conversation = ids::voice_conversation(source_id, &chat_id);
        chats.push(NormalizedChat {
            named_after: Vec::new(),
            contacts: Vec::new(),
            inputs: inputs.declared(),
            path_prefix: None,
            id: chat_id.clone(),
            chat_uuid: conversation.uuid,
            display,
            author: None,
            account: None,
            project: Some("Google Voice".to_string()),
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

fn voice_item(source_id: &str, m: &Value) -> NormalizedChatItem {
    let kind = m.get("kind").and_then(Value::as_str).unwrap_or("text");
    let mut problems = Vec::new();
    let date_ms = voice_date_ms(m, &mut problems);
    let id = ids::voice_message(
        source_id,
        m.get("id").and_then(Value::as_str).unwrap_or(""),
        date_ms,
    );
    let message_uuid = id.uuid.clone();
    let source_ref = Some(UpstreamRef::new(id.entity_kind, id.natural_key));

    let attachments: Vec<NormalizedAttachment> = voice_attachment_refs(m)
        .into_iter()
        .map(|ref_name| NormalizedAttachment {
            mime_type: mime_of(&ref_name),
            file_name: Some(ref_name.clone()),
            href: None,
            byte_len: None,
            source_url: None,
            ref_id: Some(ref_name),
        })
        .collect();

    match kind {
        "text" => {
            let is_me = m.get("is_me").and_then(Value::as_bool).unwrap_or(false);
            let sender = m.get("sender");
            let (author_display, author_kind) = if is_me {
                ("Me".to_string(), AuthorKind::Account)
            } else {
                shown(sender)
            };
            let author_handle = sender
                .and_then(|s| s.get("tel").and_then(Value::as_str))
                .filter(|_| !is_me)
                .and_then(Handle::tel);
            let body = m
                .get("body")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(escape_md_block);
            NormalizedChatItem {
                message_uuid,
                author_handle,
                author_kind,
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
                labels: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: None,
                source_ref: source_ref.clone(),
                is_aside: false,
                branch: Vec::new(),
                unread: false,
                recipients: Vec::new(),
                mentions: Vec::new(),
                problems: problems.clone(),
            }
        }
        "voicemail" | "recorded" => {
            let party = m.get("party");
            let (author_display, author_kind) = shown(party);
            let transcript = m.get("transcript").and_then(Value::as_str);
            let label = if kind == "voicemail" {
                "Voicemail"
            } else {
                "Recorded call"
            };
            let caption = match transcript {
                Some(t) if !t.is_empty() => format!("**{label}:** {}", escape_md_block(t)),
                _ => format!("**{label}**"),
            };
            NormalizedChatItem {
                message_uuid,
                author_handle: party_handle(party),
                author_kind,
                author_display,
                date_ms,
                text: Some(caption),
                // Attachment so the audio blob renders an inline player.
                kind: ItemKind::Attachment,
                attachments,
                reactions: Vec::new(),
                labels: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: None,
                source_ref: source_ref.clone(),
                is_aside: false,
                branch: Vec::new(),
                unread: false,
                recipients: Vec::new(),
                mentions: Vec::new(),
                problems: problems.clone(),
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
            let (author_display, author_kind) = shown(party);
            let system_note = Some(format!("{note} — {author_display}"));
            NormalizedChatItem {
                message_uuid,
                author_handle: party_handle(party),
                author_kind,
                author_display,
                date_ms,
                text: None,
                kind: ItemKind::System,
                attachments: Vec::new(),
                reactions: Vec::new(),
                labels: Vec::new(),
                system_note,
                source_url: None,
                kind_label: None,
                source_ref: source_ref.clone(),
                is_aside: false,
                branch: Vec::new(),
                unread: false,
                recipients: Vec::new(),
                mentions: Vec::new(),
                problems: problems.clone(),
            }
        }
    }
}

/// A Voice sender or call party as an item shows them: their name, else
/// their number, else an "Unknown" that names nobody.
fn shown(who: Option<&Value>) -> (String, AuthorKind) {
    let name = who
        .and_then(|w| w.get("name").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .or_else(|| who.and_then(|w| w.get("tel").and_then(Value::as_str)));
    match name {
        Some(name) => (name.to_string(), AuthorKind::Named),
        None => ("Unknown".to_string(), AuthorKind::Unknown),
    }
}

fn party_handle(party: Option<&Value>) -> Option<Handle> {
    party
        .and_then(|p| p.get("tel").and_then(Value::as_str))
        .and_then(Handle::tel)
}

/// Unix millis from the canonical `when` (RFC 3339), falling back to the
/// raw value, then to `None` — recorded when there was a value and it
/// would not parse.
fn voice_date_ms(m: &Value, problems: &mut Vec<Problem>) -> Option<i64> {
    let ts = m
        .get("when")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| m.get("when_raw").and_then(Value::as_str));
    own_stamp_ms(
        ts,
        "when",
        |s| {
            datalib_time::parse_strict(s)
                .ok()
                .map(|t| t.to_unix_millis())
        },
        problems,
    )
}

pub(crate) fn month_of(ms: Option<i64>) -> String {
    use chrono::TimeZone;
    chrono::Utc
        .timestamp_millis_opt(ms.unwrap_or(0))
        .single()
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn mime_of(name: &str) -> Option<String> {
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
        "mov" => "video/quicktime",
        "heic" => "image/heic",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => return None,
    };
    Some(ct.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_ids(rows: &[Value], id_key: &str) -> Vec<(String, Value)> {
        rows.iter()
            .map(|v| {
                (
                    v.get(id_key)
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    v.clone(),
                )
            })
            .collect()
    }
    use serde_json::json;

    #[test]
    fn groups_by_space_and_renders_display() {
        // Real Takeout shape: message_id is "<space>/<topic>/<message>",
        // and group_info.json has only `members` (no `name`); the space
        // id lives in the takeout directory name ("DM AAA").
        let messages = vec![
            json!({"message_id":"AAA/T1/M2","created_date":"Tuesday, February 11, 2025 at 11:34:00\u{202f}AM UTC","creator":{"name":"William Riker","email":"r@e"},"text":"Aye, sir."}),
            json!({"message_id":"AAA/T1/M1","created_date":"Tuesday, February 11, 2025 at 11:33:35\u{202f}AM UTC","creator":{"name":"Jean-Luc Picard","email":"p@e"},"text":"Set a course."}),
        ];
        let groups = vec![(
            "DM AAA".to_string(),
            json!({"members":[{"name":"Jean-Luc Picard"},{"name":"William Riker"}]}),
        )];
        let chats = chat::build("gt", &with_ids(&messages, "message_id"), &groups, &[]);
        assert_eq!(chats.len(), 1, "two messages in one space => one chat");
        assert_eq!(chats[0].id, "AAA");
        assert_eq!(chats[0].display, "Jean-Luc Picard, William Riker");
        // Both messages are the same month => one bucket, sorted oldest-first.
        assert_eq!(chats[0].buckets.len(), 1);
        assert_eq!(chats[0].buckets[0].period_key, "2025-02");
        assert_eq!(
            chats[0].buckets[0].items[0].text.as_deref(),
            Some("Set a course.")
        );
    }

    #[test]
    fn periodizes_into_month_buckets() {
        // Same space, two different months => two buckets (not one
        // monolithic "all" doc), like Signal/WhatsApp/Voice.
        let messages = vec![
            json!({"message_id":"BBB/T1/M1","created_date":"Tuesday, February 11, 2025 at 11:33:35\u{202f}AM UTC","creator":{"name":"Picard","email":"p@e"},"text":"Feb."}),
            json!({"message_id":"BBB/T2/M2","created_date":"Wednesday, March 5, 2025 at 9:34:00\u{202f}PM UTC","creator":{"name":"Riker","email":"r@e"},"text":"March."}),
        ];
        let groups: Vec<(String, Value)> = vec![];
        let chats = chat::build("gt", &with_ids(&messages, "message_id"), &groups, &[]);
        assert_eq!(chats.len(), 1);
        let keys: Vec<&str> = chats[0]
            .buckets
            .iter()
            .map(|b| b.period_key.as_str())
            .collect();
        assert_eq!(keys, vec!["2025-02", "2025-03"]);
        // Falls back to the bare space id when no group dir provided members.
        assert_eq!(chats[0].display, "BBB");
    }

    #[test]
    fn space_prefix() {
        // Real shape: first path segment is the space id.
        assert_eq!(chat::space_of("AAA/T1/M1"), "AAA");
        assert_eq!(chat::space_of("weird"), "weird");
        // Dir name carries the space id in its trailing token.
        assert_eq!(chat::space_of_dir("DM AAA"), "AAA");
        assert_eq!(chat::space_of_dir("Space FooBar"), "FooBar");
    }

    #[test]
    fn parse_date_ms_handles_narrow_no_break_space() {
        let feb = chat::parse_date_ms("Tuesday, February 11, 2025 at 11:33:35\u{202f}AM UTC");
        assert!(
            feb.is_some_and(|ms| ms > 0),
            "narrow no-break space must still parse"
        );
        assert_eq!(month_of(feb), "2025-02");
    }

    /// A Chat message dated in a zone outside North America got its time
    /// from the ingest and none from the render, which read only `UTC`.
    #[test]
    fn a_chat_date_in_any_zone_the_ingest_reads_parses() {
        assert_eq!(
            chat::parse_date_ms("Tuesday, February 11, 2025 at 11:33:35 AM CET"),
            chat::parse_date_ms("Tuesday, February 11, 2025 at 10:33:35 AM UTC"),
        );
        assert!(
            chat::parse_date_ms("Tuesday, February 11, 2025 at 11:33:35 AM GMT+5:30").is_some()
        );
    }

    /// A shape we don't recognize must produce no timestamp, never the
    /// epoch — the doc comment on `parse_date_ms` used to promise `0`,
    /// and those rows sorted into the grid as real 1970 records.
    #[test]
    fn unparseable_dates_yield_none_not_the_epoch() {
        for bad in [
            "",
            "not a date",
            // Right words, wrong shape (no weekday, no "at").
            "February 11, 2025 11:33:35 AM UTC",
            // A zone name two zones share — refusing is the point.
            "Tuesday, February 11, 2025 at 11:33:35 AM IST",
        ] {
            assert_eq!(
                chat::parse_date_ms(bad),
                None,
                "chat::parse_date_ms({bad:?}) fabricated a stamp"
            );
        }
        // …and an undated item still files, under the epoch bucket.
        assert_eq!(month_of(None), "1970-01");
    }

    #[test]
    fn voice_date_ms_yields_none_when_undated() {
        assert_eq!(voice_date_ms(&json!({}), &mut Vec::new()), None);
        assert_eq!(voice_date_ms(&json!({"when": ""}), &mut Vec::new()), None);
        assert_eq!(
            voice_date_ms(&json!({"when": "yesterday"}), &mut Vec::new()),
            None
        );
        assert_eq!(
            voice_date_ms(
                &json!({"when": "2364-03-01T09:00:00.742-08:00"}),
                &mut Vec::new()
            ),
            Some(12_438_637_200_742),
        );
    }

    #[test]
    fn voice_groups_by_contact_and_buckets_by_month() {
        let messages = vec![
            json!({"id":"u1","kind":"text","conversation_key":"+12025550102","conversation_display":"William Riker","when":"2364-03-01T09:00:00.742-08:00","sender":{"tel":"+12025550102","name":"William Riker"},"is_me":false,"body":"Hello","attachments":[]}),
            json!({"id":"u2","kind":"text","conversation_key":"+12025550102","conversation_display":"+12025550102","when":"2364-04-02T10:00:00.000-08:00","sender":{"tel":"+12025550100","name":null},"is_me":true,"body":"Hi back","attachments":[]}),
        ];
        let chats = build_voice_chats("gt", &with_ids(&messages, "id"));
        assert_eq!(chats.len(), 1);
        // Human name wins over the bare-number display.
        assert_eq!(chats[0].display, "William Riker");
        assert_eq!(chats[0].id, "voice:+12025550102");
        // Two distinct months → two buckets.
        assert_eq!(chats[0].buckets.len(), 2);
        assert_eq!(chats[0].buckets[0].period_key, "2364-03");
        assert_eq!(chats[0].buckets[1].period_key, "2364-04");
        // is_me → "Me".
        assert_eq!(chats[0].buckets[1].items[0].author_display, "Me");
        assert_eq!(
            chats[0].buckets[1].items[0].author_kind,
            AuthorKind::Account
        );
    }

    /// A text and a transcript are what was said; the bold label around
    /// the transcript is ours.
    #[test]
    fn voice_text_in_markup_renders_escaped() {
        let messages = vec![
            json!({
                "id":"t1","kind":"text","conversation_key":"+1555","conversation_display":"Q",
                "when":"2364-02-18T16:10:05.000-08:00","sender":{"tel":"+1555","name":"Q"},
                "body":"<script>x</script> & co"
            }),
            json!({
                "id":"v1","kind":"voicemail","conversation_key":"+1555","conversation_display":"Q",
                "when":"2364-02-18T16:11:05.000-08:00","party":{"tel":"+1555","name":"Q"},
                "transcript":"<script>x</script> & co","audio":"vm.mp3"
            }),
        ];
        let chats = build_voice_chats("gt", &with_ids(&messages, "id"));
        let items = &chats[0].buckets[0].items;
        assert_eq!(
            items[0].text.as_deref(),
            Some("&lt;script&gt;x&lt;/script&gt; &amp; co")
        );
        assert_eq!(
            items[1].text.as_deref(),
            Some("**Voicemail:** &lt;script&gt;x&lt;/script&gt; &amp; co")
        );
    }

    #[test]
    fn voicemail_is_attachment_with_transcript_caption() {
        let messages = vec![json!({
            "id":"v1","kind":"voicemail","conversation_key":"+1555","conversation_display":"Jean-Luc Picard",
            "when":"2364-02-18T16:10:05.000-08:00","party":{"tel":"+1555","name":"Jean-Luc Picard"},
            "transcript":"Make it so.","duration":"PT13S","audio":"vm.mp3"
        })];
        let chats = build_voice_chats("gt", &with_ids(&messages, "id"));
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
            "when":"2364-03-06T09:50:34.000-08:00","party":{"tel":"+1999","name":"Spammer"}
        })];
        let chats = build_voice_chats("gt", &with_ids(&messages, "id"));
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::System);
        assert_eq!(item.system_note.as_deref(), Some("Missed call — Spammer"));
    }

    #[test]
    fn voice_mms_text_becomes_attachment_item() {
        let messages = vec![json!({
            "id":"t1","kind":"text","conversation_key":"+1202","conversation_display":"+1202",
            "when":"2364-03-02T09:06:01.024-08:00","sender":{"tel":"+1202","name":null},"is_me":false,
            "body":"pic","attachments":["+1202 - Text - x-1-1.jpg"]
        })];
        let chats = build_voice_chats("gt", &with_ids(&messages, "id"));
        let item = &chats[0].buckets[0].items[0];
        assert_eq!(item.kind, ItemKind::Attachment);
        assert_eq!(item.attachments[0].mime_type.as_deref(), Some("image/jpeg"));
    }
}
