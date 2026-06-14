//! Decrypt → mirror → commit for a single WhatsApp backup directory.
//!
//! Entry point is [`ingest`]: given `backup_dir`
//! (containing `Databases/msgstore.db.crypt15` and `Media/`), the
//! 32-byte root key, and a target `wa_raw.doltlite_db` path, decrypts
//! the message store to a tempfile, walks the curated tables into the
//! target db (drop-and-rebuild), registers media files by sha256, and
//! issues a single `dolt_commit`.
//!
//! The decrypted msgstore lives in a `tempfile::NamedTempFile` and is
//! dropped at the end of `ingest`; the plaintext never touches a
//! user-visible path. Media files are read directly from
//! `backup_dir/Media/` (WhatsApp stores them in the clear) so no
//! plaintext copy of those is ever made.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, blake3_hex, BlobCas, CasInsert};
use frankweiler_etl::doltlite_raw;
use frankweiler_whatsapp_backup::decrypt_file;

use crate::schema_raw::{ALL_DDL, DATA_TABLES};

#[derive(Debug, Clone, Default)]
pub struct IngestSummary {
    pub jids: u64,
    pub chats: u64,
    pub messages: u64,
    pub message_text: u64,
    pub message_media: u64,
    pub message_add_on: u64,
    pub message_add_on_reaction: u64,
    pub media_files: u64,
    /// `dolt_commit` returned a non-empty hash (something actually
    /// changed). False on a clean re-run of the same backup — drop-and-
    /// rebuild produces an empty commit and we skip it.
    pub committed: bool,
}

/// Thin wrapper over the doltlite raw-store pool, mirroring the
/// `RawDb` pattern every other provider uses. Lets the sync
/// orchestrator open the pool once at the start of an extract run
/// (so SIGINT can flush in-flight stores) and pass the same handle
/// into `ingest`.
#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    /// Path on disk of the doltlite file the pool wraps. Kept so
    /// `frankweiler_etl::blob_cas::cas_path_for` can derive the
    /// sibling CAS file location without the caller threading two
    /// paths around in parallel.
    db_path: PathBuf,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = doltlite_raw::open(db_path, ALL_DDL).await?;
        Ok(Self {
            pool,
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Full pipeline: decrypt, mirror, commit.
///
/// `backup_dir` must contain `Databases/msgstore.db.crypt15`. If a
/// sibling `Media/` directory exists, every file under it is registered
/// in `wa_media_files`; absent silently means "no media to register".
///
/// `target_db_path` is the doltlite file to populate. Created if absent;
/// extended in-place if present (drop-and-rebuild of the `wa_*` tables).
pub async fn ingest(
    backup_dir: &Path,
    root_key: &[u8; 32],
    target_db_path: &Path,
) -> Result<IngestSummary> {
    let db = RawDb::open(target_db_path).await?;
    fetch(backup_dir, root_key, &db).await
}

/// Variant of [`ingest`] that takes an already-open [`RawDb`]. Used by
/// the sync orchestrator, which opens the pool up front (so SIGINT can
/// flush) and threads it through.
pub async fn fetch(backup_dir: &Path, root_key: &[u8; 32], db: &RawDb) -> Result<IngestSummary> {
    fetch_with_pool(backup_dir, root_key, db.pool().clone(), db.db_path()).await
}

async fn fetch_with_pool(
    backup_dir: &Path,
    root_key: &[u8; 32],
    dst_pool: SqlitePool,
    target_db_path: &Path,
) -> Result<IngestSummary> {
    // `backup_dir` lives inside the `sync:` block (not on
    // SourceCommon.input_path), so core's load-time tilde expansion
    // doesn't touch it — it lands here as a literal `~/...` if the
    // user wrote `backup_dir: ~/backups/WhatsApp` in YAML. Expand
    // here, same way signal handles `snapshot_dir`.
    let backup_dir = expand_tilde(backup_dir);
    let backup_dir = backup_dir.as_path();
    let crypt_path = backup_dir.join("Databases").join("msgstore.db.crypt15");
    tracing::info!(
        crypt_path = %crypt_path.display(),
        "whatsapp::ingest start"
    );

    let plaintext = decrypt_file(&crypt_path, root_key)
        .with_context(|| format!("decrypt {}", crypt_path.display()))?;
    tracing::info!(
        decrypted_bytes = plaintext.len(),
        "whatsapp::ingest: msgstore decrypted"
    );

    let tmp = tempfile::Builder::new()
        .prefix("wa-msgstore-")
        .suffix(".db")
        .tempfile()
        .context("create tempfile for decrypted msgstore")?;
    std::fs::write(tmp.path(), &plaintext).context("write decrypted msgstore to tempfile")?;
    // Free the heap copy — the file on disk is the working source now.
    drop(plaintext);

    let src_pool = open_source_sqlite(tmp.path()).await?;

    truncate_wa_tables(&dst_pool).await?;

    let mut summary = IngestSummary::default();
    let jid_map = mirror_jid(&src_pool, &dst_pool, &mut summary).await?;
    let chat_map = mirror_chat(&src_pool, &dst_pool, &jid_map, &mut summary).await?;
    let msg_map = mirror_message(&src_pool, &dst_pool, &jid_map, &chat_map, &mut summary).await?;
    mirror_message_text(&src_pool, &dst_pool, &msg_map, &mut summary).await?;
    mirror_message_media(&src_pool, &dst_pool, &msg_map, &mut summary).await?;
    let addon_map = mirror_message_add_on(
        &src_pool,
        &dst_pool,
        &jid_map,
        &chat_map,
        &msg_map,
        &mut summary,
    )
    .await?;
    mirror_message_add_on_reaction(&src_pool, &dst_pool, &addon_map, &mut summary).await?;

    let media_root = backup_dir.join("Media");
    if media_root.is_dir() {
        mirror_media_files(&dst_pool, target_db_path, &media_root, &mut summary).await?;
    } else {
        tracing::info!(
            media_root = %media_root.display(),
            "whatsapp::ingest: no Media/ dir; skipping media-file registry"
        );
    }

    summary.committed = commit_if_dirty(&dst_pool).await?;

    tracing::info!(?summary, "whatsapp::ingest done");
    Ok(summary)
}

/// Sqlite source-side pool (the decrypted msgstore.db). Read-only,
/// single connection.
async fn open_source_sqlite(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("sqlite uri for {}", path.display()))?
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open source sqlite at {}", path.display()))
}

async fn truncate_wa_tables(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin().await.context("begin truncate tx")?;
    for table in DATA_TABLES {
        let sql = format!("DELETE FROM {table}");
        sqlx::query(&sql)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("truncate {table}"))?;
    }
    tx.commit().await.context("commit truncate tx")?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Per-table mirrors
// ─────────────────────────────────────────────────────────────────────

/// jid: source `_id` → raw_string lookup table for rekey of every
/// `*_jid_row_id` column the other tables carry.
async fn mirror_jid(
    src: &SqlitePool,
    dst: &SqlitePool,
    summary: &mut IngestSummary,
) -> Result<HashMap<i64, String>> {
    let rows = sqlx::query("SELECT _id, user, server, agent, device, type, raw_string FROM jid")
        .fetch_all(src)
        .await
        .context("select jid")?;
    let mut map = HashMap::with_capacity(rows.len());
    let mut tx = dst.begin().await.context("begin wa_jid tx")?;
    for r in &rows {
        let id: i64 = r.get("_id");
        let user: String = r.get("user");
        let server: String = r.get("server");
        let agent: Option<i64> = r.get("agent");
        let device: Option<i64> = r.get("device");
        let ty: Option<i64> = r.get("type");
        let raw_string: Option<String> = r.get("raw_string");
        // Some seed rows in jid have NULL raw_string (synthetic
        // placeholders). Synthesize from user@server so we never store
        // a NULL PK and the map still resolves to *some* stable key.
        let raw_string = raw_string.unwrap_or_else(|| format!("{user}@{server}"));
        sqlx::query(
            "INSERT INTO wa_jid (raw_string, user, server, agent, device, type) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&raw_string)
        .bind(&user)
        .bind(&server)
        .bind(agent)
        .bind(device)
        .bind(ty)
        .execute(&mut *tx)
        .await
        .context("insert wa_jid")?;
        map.insert(id, raw_string);
    }
    tx.commit().await.context("commit wa_jid tx")?;
    summary.jids = rows.len() as u64;
    Ok(map)
}

/// chat: source `_id` → chat_jid (= jid_map[chat.jid_row_id]).
async fn mirror_chat(
    src: &SqlitePool,
    dst: &SqlitePool,
    jid_map: &HashMap<i64, String>,
    summary: &mut IngestSummary,
) -> Result<HashMap<i64, String>> {
    let rows = sqlx::query(
        "SELECT _id, jid_row_id, hidden, subject, created_timestamp, archived, sort_timestamp, \
                mod_tag, gen, spam_detection, unseen_earliest_message_received_time, \
                unseen_message_count, unseen_missed_calls_count, unseen_row_count, \
                plaintext_disabled, vcard_ui_dismissed, show_group_description, \
                ephemeral_expiration, ephemeral_setting_timestamp, ephemeral_displayed_exemptions, \
                ephemeral_disappearing_messages_initiator, unseen_important_message_count, \
                group_type, unseen_message_reaction_count, unseen_comment_message_count, \
                growth_lock_level, growth_lock_expiration_ts, \
                has_new_community_admin_dialog_been_acknowledged, history_sync_progress, \
                chat_lock, chat_origin, participation_status, account_jid_row_id, \
                chat_encryption_state, group_member_count, limited_sharing, \
                limited_sharing_setting_timestamp, is_contact, ephemeral_after_read_duration, \
                business_chat_state \
         FROM chat",
    )
    .fetch_all(src)
    .await
    .context("select chat")?;
    let mut map = HashMap::with_capacity(rows.len());
    let mut tx = dst.begin().await.context("begin wa_chat tx")?;
    for r in &rows {
        let id: i64 = r.get("_id");
        let jid_row_id: Option<i64> = r.get("jid_row_id");
        let Some(chat_jid) = jid_row_id.and_then(|i| jid_map.get(&i).cloned()) else {
            tracing::warn!(
                chat_id = id,
                "wa_chat: jid_row_id not in jid map; dropping row"
            );
            continue;
        };
        let account_jid_row_id: Option<i64> = r.get("account_jid_row_id");
        let account_jid = account_jid_row_id.and_then(|i| jid_map.get(&i).cloned());
        sqlx::query(
            "INSERT INTO wa_chat (chat_jid, hidden, subject, created_timestamp, archived, \
                sort_timestamp, mod_tag, gen, spam_detection, \
                unseen_earliest_message_received_time, unseen_message_count, \
                unseen_missed_calls_count, unseen_row_count, plaintext_disabled, \
                vcard_ui_dismissed, show_group_description, ephemeral_expiration, \
                ephemeral_setting_timestamp, ephemeral_displayed_exemptions, \
                ephemeral_disappearing_messages_initiator, unseen_important_message_count, \
                group_type, unseen_message_reaction_count, unseen_comment_message_count, \
                growth_lock_level, growth_lock_expiration_ts, \
                has_new_community_admin_dialog_been_acknowledged, history_sync_progress, \
                chat_lock, chat_origin, participation_status, account_jid, \
                chat_encryption_state, group_member_count, limited_sharing, \
                limited_sharing_setting_timestamp, is_contact, ephemeral_after_read_duration, \
                business_chat_state) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&chat_jid)
        .bind(r.get::<Option<i64>, _>("hidden"))
        .bind(r.get::<Option<String>, _>("subject"))
        .bind(r.get::<Option<i64>, _>("created_timestamp"))
        .bind(r.get::<Option<i64>, _>("archived"))
        .bind(r.get::<Option<i64>, _>("sort_timestamp"))
        .bind(r.get::<Option<i64>, _>("mod_tag"))
        .bind(r.get::<Option<f64>, _>("gen"))
        .bind(r.get::<Option<i64>, _>("spam_detection"))
        .bind(r.get::<Option<i64>, _>("unseen_earliest_message_received_time"))
        .bind(r.get::<Option<i64>, _>("unseen_message_count"))
        .bind(r.get::<Option<i64>, _>("unseen_missed_calls_count"))
        .bind(r.get::<Option<i64>, _>("unseen_row_count"))
        .bind(r.get::<Option<i64>, _>("plaintext_disabled"))
        .bind(r.get::<Option<i64>, _>("vcard_ui_dismissed"))
        .bind(r.get::<Option<i64>, _>("show_group_description"))
        .bind(r.get::<Option<i64>, _>("ephemeral_expiration"))
        .bind(r.get::<Option<i64>, _>("ephemeral_setting_timestamp"))
        .bind(r.get::<Option<i64>, _>("ephemeral_displayed_exemptions"))
        .bind(r.get::<Option<i64>, _>("ephemeral_disappearing_messages_initiator"))
        .bind(r.get::<Option<i64>, _>("unseen_important_message_count"))
        .bind(r.get::<Option<i64>, _>("group_type"))
        .bind(r.get::<Option<i64>, _>("unseen_message_reaction_count"))
        .bind(r.get::<Option<i64>, _>("unseen_comment_message_count"))
        .bind(r.get::<Option<i64>, _>("growth_lock_level"))
        .bind(r.get::<Option<i64>, _>("growth_lock_expiration_ts"))
        .bind(r.get::<Option<i64>, _>("has_new_community_admin_dialog_been_acknowledged"))
        .bind(r.get::<Option<i64>, _>("history_sync_progress"))
        .bind(r.get::<Option<i64>, _>("chat_lock"))
        .bind(r.get::<Option<String>, _>("chat_origin"))
        .bind(r.get::<Option<i64>, _>("participation_status"))
        .bind(account_jid)
        .bind(r.get::<Option<i64>, _>("chat_encryption_state"))
        .bind(r.get::<Option<i64>, _>("group_member_count"))
        .bind(r.get::<Option<i64>, _>("limited_sharing"))
        .bind(r.get::<Option<i64>, _>("limited_sharing_setting_timestamp"))
        .bind(r.get::<Option<i64>, _>("is_contact"))
        .bind(r.get::<Option<i64>, _>("ephemeral_after_read_duration"))
        .bind(r.get::<Option<i64>, _>("business_chat_state"))
        .execute(&mut *tx)
        .await
        .context("insert wa_chat")?;
        map.insert(id, chat_jid);
    }
    tx.commit().await.context("commit wa_chat tx")?;
    summary.chats = map.len() as u64;
    Ok(map)
}

/// Triple identifying a message in the rekeyed schema.
#[derive(Debug, Clone)]
pub struct MsgKey {
    pub chat_jid: String,
    pub key_id: String,
    pub from_me: i64,
}

async fn mirror_message(
    src: &SqlitePool,
    dst: &SqlitePool,
    jid_map: &HashMap<i64, String>,
    chat_map: &HashMap<i64, String>,
    summary: &mut IngestSummary,
) -> Result<HashMap<i64, MsgKey>> {
    let rows = sqlx::query(
        "SELECT _id, chat_row_id, from_me, key_id, sender_jid_row_id, status, broadcast, \
                recipient_count, participant_hash, origination_flags, origin, timestamp, \
                received_timestamp, receipt_server_timestamp, message_type, text_data, starred, \
                lookup_tables, message_add_on_flags, view_mode, sort_id, translated_text, \
                server_sts \
         FROM message",
    )
    .fetch_all(src)
    .await
    .context("select message")?;
    let mut map = HashMap::with_capacity(rows.len());
    let mut tx = dst.begin().await.context("begin wa_message tx")?;
    for r in &rows {
        let id: i64 = r.get("_id");
        let chat_row_id: i64 = r.get("chat_row_id");
        let from_me: i64 = r.get("from_me");
        let key_id: String = r.get("key_id");
        let Some(chat_jid) = chat_map.get(&chat_row_id).cloned() else {
            // Every msgstore ships with a synthetic seed row at
            // `_id=1` with `chat_row_id=-1` and `key_id="-1"` —
            // it's an Android-side schema artifact, not a real
            // message. Drop it silently. Other orphan rows
            // (real `key_id`, missing chat) still WARN because
            // they're worth flagging — maybe a chat row was
            // pruned, maybe the source is corrupted.
            if chat_row_id == -1 && key_id == "-1" {
                tracing::debug!(message_id = id, "wa_message: dropping msgstore seed row");
            } else {
                tracing::warn!(
                    message_id = id,
                    chat_row_id,
                    key_id,
                    "wa_message: chat_row_id not in chat map; dropping row"
                );
            }
            continue;
        };
        let sender_jid_row_id: Option<i64> = r.get("sender_jid_row_id");
        let sender_jid = sender_jid_row_id.and_then(|i| jid_map.get(&i).cloned());
        sqlx::query(
            "INSERT OR IGNORE INTO wa_message (chat_jid, key_id, from_me, sender_jid, status, \
                broadcast, recipient_count, participant_hash, origination_flags, origin, \
                timestamp, received_timestamp, receipt_server_timestamp, message_type, \
                text_data, starred, lookup_tables, message_add_on_flags, view_mode, sort_id, \
                translated_text, server_sts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&chat_jid)
        .bind(&key_id)
        .bind(from_me)
        .bind(sender_jid)
        .bind(r.get::<Option<i64>, _>("status"))
        .bind(r.get::<Option<i64>, _>("broadcast"))
        .bind(r.get::<Option<i64>, _>("recipient_count"))
        .bind(r.get::<Option<String>, _>("participant_hash"))
        .bind(r.get::<Option<i64>, _>("origination_flags"))
        .bind(r.get::<Option<i64>, _>("origin"))
        .bind(r.get::<Option<i64>, _>("timestamp"))
        .bind(r.get::<Option<i64>, _>("received_timestamp"))
        .bind(r.get::<Option<i64>, _>("receipt_server_timestamp"))
        .bind(r.get::<Option<i64>, _>("message_type"))
        .bind(r.get::<Option<String>, _>("text_data"))
        .bind(r.get::<Option<i64>, _>("starred"))
        .bind(r.get::<Option<i64>, _>("lookup_tables"))
        .bind(r.get::<Option<i64>, _>("message_add_on_flags"))
        .bind(r.get::<Option<i64>, _>("view_mode"))
        .bind(r.get::<i64, _>("sort_id"))
        .bind(r.get::<Option<String>, _>("translated_text"))
        .bind(r.get::<Option<i64>, _>("server_sts"))
        .execute(&mut *tx)
        .await
        .context("insert wa_message")?;
        map.insert(
            id,
            MsgKey {
                chat_jid,
                key_id,
                from_me,
            },
        );
    }
    tx.commit().await.context("commit wa_message tx")?;
    summary.messages = map.len() as u64;
    Ok(map)
}

async fn mirror_message_text(
    src: &SqlitePool,
    dst: &SqlitePool,
    msg_map: &HashMap<i64, MsgKey>,
    summary: &mut IngestSummary,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT message_row_id, description, page_title, url, font_style, text_color, \
                background_color, preview_type, invite_link_group_type, counter_abuse_token, \
                fb_experiment_id, social_media_post_type, link_media_duration_seconds, \
                link_end_index \
         FROM message_text",
    )
    .fetch_all(src)
    .await
    .context("select message_text")?;
    let mut tx = dst.begin().await.context("begin wa_message_text tx")?;
    let mut n = 0u64;
    for r in &rows {
        let message_row_id: i64 = r.get("message_row_id");
        let Some(k) = msg_map.get(&message_row_id) else {
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO wa_message_text (chat_jid, key_id, from_me, description, \
                page_title, url, font_style, text_color, background_color, preview_type, \
                invite_link_group_type, counter_abuse_token, fb_experiment_id, \
                social_media_post_type, link_media_duration_seconds, link_end_index) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&k.chat_jid)
        .bind(&k.key_id)
        .bind(k.from_me)
        .bind(r.get::<Option<String>, _>("description"))
        .bind(r.get::<Option<String>, _>("page_title"))
        .bind(r.get::<Option<String>, _>("url"))
        .bind(r.get::<Option<i64>, _>("font_style"))
        .bind(r.get::<Option<i64>, _>("text_color"))
        .bind(r.get::<Option<i64>, _>("background_color"))
        .bind(r.get::<Option<i64>, _>("preview_type"))
        .bind(r.get::<Option<i64>, _>("invite_link_group_type"))
        .bind(r.get::<Option<String>, _>("counter_abuse_token"))
        .bind(r.get::<Option<i64>, _>("fb_experiment_id"))
        .bind(r.get::<Option<i64>, _>("social_media_post_type"))
        .bind(r.get::<Option<i64>, _>("link_media_duration_seconds"))
        .bind(r.get::<Option<i64>, _>("link_end_index"))
        .execute(&mut *tx)
        .await
        .context("insert wa_message_text")?;
        n += 1;
    }
    tx.commit().await.context("commit wa_message_text tx")?;
    summary.message_text = n;
    Ok(())
}

async fn mirror_message_media(
    src: &SqlitePool,
    dst: &SqlitePool,
    msg_map: &HashMap<i64, MsgKey>,
    summary: &mut IngestSummary,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT message_row_id, autotransfer_retry_enabled, transferred, face_x, face_y, \
                has_streaming_sidecar, page_count, thumbnail_height_width_ratio, \
                first_scan_sidecar, first_scan_length, message_url, media_upload_handle, \
                sticker_flags, raw_transcription_text, first_viewed_timestamp, \
                is_animated_sticker, premium_message, media_caption, metadata_url, \
                motion_photo_presentation_offset_ms, qr_url, media_key_domain, e2ee_media_key, \
                emoji_tags, multicast_id, media_job_uuid, transcoded, file_path, file_size, \
                suspicious_content, trim_from, trim_to, media_key, media_key_timestamp, width, \
                height, gif_attribution, direct_path, mime_type, file_length, media_name, \
                file_hash, media_duration, enc_file_hash, partial_media_hash, \
                partial_media_enc_hash, original_file_hash, mute_video, doodle_id, \
                media_source_type, accessibility_label, media_transcode_quality, is_offloaded \
         FROM message_media",
    )
    .fetch_all(src)
    .await
    .context("select message_media")?;
    let mut tx = dst.begin().await.context("begin wa_message_media tx")?;
    let mut n = 0u64;
    for r in &rows {
        let message_row_id: i64 = r.get("message_row_id");
        let Some(k) = msg_map.get(&message_row_id) else {
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO wa_message_media (chat_jid, key_id, from_me, \
                autotransfer_retry_enabled, transferred, face_x, face_y, has_streaming_sidecar, \
                page_count, thumbnail_height_width_ratio, first_scan_sidecar, first_scan_length, \
                message_url, media_upload_handle, sticker_flags, raw_transcription_text, \
                first_viewed_timestamp, is_animated_sticker, premium_message, media_caption, \
                metadata_url, motion_photo_presentation_offset_ms, qr_url, media_key_domain, \
                e2ee_media_key, emoji_tags, multicast_id, media_job_uuid, transcoded, file_path, \
                file_size, suspicious_content, trim_from, trim_to, media_key, \
                media_key_timestamp, width, height, gif_attribution, direct_path, mime_type, \
                file_length, media_name, file_hash, media_duration, enc_file_hash, \
                partial_media_hash, partial_media_enc_hash, original_file_hash, mute_video, \
                doodle_id, media_source_type, accessibility_label, media_transcode_quality, \
                is_offloaded) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     ?, ?, ?, ?, ?)",
        )
        .bind(&k.chat_jid)
        .bind(&k.key_id)
        .bind(k.from_me)
        .bind(r.get::<Option<i64>, _>("autotransfer_retry_enabled"))
        .bind(r.get::<Option<i64>, _>("transferred"))
        .bind(r.get::<Option<i64>, _>("face_x"))
        .bind(r.get::<Option<i64>, _>("face_y"))
        .bind(r.get::<Option<i64>, _>("has_streaming_sidecar"))
        .bind(r.get::<Option<i64>, _>("page_count"))
        .bind(r.get::<Option<f64>, _>("thumbnail_height_width_ratio"))
        .bind(r.get::<Option<Vec<u8>>, _>("first_scan_sidecar"))
        .bind(r.get::<Option<i64>, _>("first_scan_length"))
        .bind(r.get::<Option<String>, _>("message_url"))
        .bind(r.get::<Option<String>, _>("media_upload_handle"))
        .bind(r.get::<Option<i64>, _>("sticker_flags"))
        .bind(r.get::<Option<String>, _>("raw_transcription_text"))
        .bind(r.get::<Option<i64>, _>("first_viewed_timestamp"))
        .bind(r.get::<Option<i64>, _>("is_animated_sticker"))
        .bind(r.get::<Option<i64>, _>("premium_message"))
        .bind(r.get::<Option<String>, _>("media_caption"))
        .bind(r.get::<Option<String>, _>("metadata_url"))
        .bind(r.get::<Option<i64>, _>("motion_photo_presentation_offset_ms"))
        .bind(r.get::<Option<String>, _>("qr_url"))
        .bind(r.get::<Option<i64>, _>("media_key_domain"))
        .bind(r.get::<Option<Vec<u8>>, _>("e2ee_media_key"))
        .bind(r.get::<Option<String>, _>("emoji_tags"))
        .bind(r.get::<Option<String>, _>("multicast_id"))
        .bind(r.get::<Option<String>, _>("media_job_uuid"))
        .bind(r.get::<Option<i64>, _>("transcoded"))
        .bind(r.get::<Option<String>, _>("file_path"))
        .bind(r.get::<Option<i64>, _>("file_size"))
        .bind(r.get::<Option<i64>, _>("suspicious_content"))
        .bind(r.get::<Option<i64>, _>("trim_from"))
        .bind(r.get::<Option<i64>, _>("trim_to"))
        .bind(r.get::<Option<Vec<u8>>, _>("media_key"))
        .bind(r.get::<Option<i64>, _>("media_key_timestamp"))
        .bind(r.get::<Option<i64>, _>("width"))
        .bind(r.get::<Option<i64>, _>("height"))
        .bind(r.get::<Option<i64>, _>("gif_attribution"))
        .bind(r.get::<Option<String>, _>("direct_path"))
        .bind(r.get::<Option<String>, _>("mime_type"))
        .bind(r.get::<Option<i64>, _>("file_length"))
        .bind(r.get::<Option<String>, _>("media_name"))
        .bind(r.get::<Option<String>, _>("file_hash"))
        .bind(r.get::<Option<i64>, _>("media_duration"))
        .bind(r.get::<Option<String>, _>("enc_file_hash"))
        .bind(r.get::<Option<String>, _>("partial_media_hash"))
        .bind(r.get::<Option<String>, _>("partial_media_enc_hash"))
        .bind(r.get::<Option<String>, _>("original_file_hash"))
        .bind(r.get::<Option<i64>, _>("mute_video"))
        .bind(r.get::<Option<String>, _>("doodle_id"))
        .bind(r.get::<Option<i64>, _>("media_source_type"))
        .bind(r.get::<Option<String>, _>("accessibility_label"))
        .bind(r.get::<Option<i64>, _>("media_transcode_quality"))
        .bind(r.get::<Option<i64>, _>("is_offloaded"))
        .execute(&mut *tx)
        .await
        .context("insert wa_message_media")?;
        n += 1;
    }
    tx.commit().await.context("commit wa_message_media tx")?;
    summary.message_media = n;
    Ok(())
}

async fn mirror_message_add_on(
    src: &SqlitePool,
    dst: &SqlitePool,
    jid_map: &HashMap<i64, String>,
    chat_map: &HashMap<i64, String>,
    msg_map: &HashMap<i64, MsgKey>,
    summary: &mut IngestSummary,
) -> Result<HashMap<i64, MsgKey>> {
    let rows = sqlx::query(
        "SELECT _id, chat_row_id, from_me, key_id, sender_jid_row_id, parent_message_row_id, \
                timestamp, status, message_add_on_type, received_timestamp, \
                expiry_duration_in_secs, server_timestamp, expiry_timestamp, expiry_type \
         FROM message_add_on",
    )
    .fetch_all(src)
    .await
    .context("select message_add_on")?;
    let mut tx = dst.begin().await.context("begin wa_message_add_on tx")?;
    let mut addon_map = HashMap::with_capacity(rows.len());
    let mut n = 0u64;
    for r in &rows {
        let id: i64 = r.get("_id");
        let chat_row_id: Option<i64> = r.get("chat_row_id");
        let from_me: Option<i64> = r.get("from_me");
        let key_id: String = r.get("key_id");
        let Some(chat_jid) = chat_row_id.and_then(|i| chat_map.get(&i).cloned()) else {
            tracing::warn!(
                add_on_id = id,
                "wa_message_add_on: chat_row_id not in chat map; dropping row"
            );
            continue;
        };
        let from_me = from_me.unwrap_or(0);
        let sender_jid = r
            .get::<Option<i64>, _>("sender_jid_row_id")
            .and_then(|i| jid_map.get(&i).cloned());
        let parent_key = r
            .get::<Option<i64>, _>("parent_message_row_id")
            .and_then(|i| msg_map.get(&i));
        let (parent_chat_jid, parent_key_id, parent_from_me) = match parent_key {
            Some(k) => (
                Some(k.chat_jid.clone()),
                Some(k.key_id.clone()),
                Some(k.from_me),
            ),
            None => (None, None, None),
        };
        sqlx::query(
            "INSERT OR IGNORE INTO wa_message_add_on (chat_jid, key_id, from_me, sender_jid, \
                parent_chat_jid, parent_key_id, parent_from_me, timestamp, status, \
                message_add_on_type, received_timestamp, expiry_duration_in_secs, \
                server_timestamp, expiry_timestamp, expiry_type) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&chat_jid)
        .bind(&key_id)
        .bind(from_me)
        .bind(sender_jid)
        .bind(parent_chat_jid)
        .bind(parent_key_id)
        .bind(parent_from_me)
        .bind(r.get::<Option<i64>, _>("timestamp"))
        .bind(r.get::<Option<i64>, _>("status"))
        .bind(r.get::<Option<i64>, _>("message_add_on_type"))
        .bind(r.get::<Option<i64>, _>("received_timestamp"))
        .bind(r.get::<Option<i64>, _>("expiry_duration_in_secs"))
        .bind(r.get::<Option<i64>, _>("server_timestamp"))
        .bind(r.get::<Option<i64>, _>("expiry_timestamp"))
        .bind(r.get::<Option<i64>, _>("expiry_type"))
        .execute(&mut *tx)
        .await
        .context("insert wa_message_add_on")?;
        addon_map.insert(
            id,
            MsgKey {
                chat_jid,
                key_id,
                from_me,
            },
        );
        n += 1;
    }
    tx.commit().await.context("commit wa_message_add_on tx")?;
    summary.message_add_on = n;
    Ok(addon_map)
}

async fn mirror_message_add_on_reaction(
    src: &SqlitePool,
    dst: &SqlitePool,
    addon_map: &HashMap<i64, MsgKey>,
    summary: &mut IngestSummary,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT message_add_on_row_id, reaction, sender_timestamp FROM message_add_on_reaction",
    )
    .fetch_all(src)
    .await
    .context("select message_add_on_reaction")?;
    let mut tx = dst
        .begin()
        .await
        .context("begin wa_message_add_on_reaction tx")?;
    let mut n = 0u64;
    for r in &rows {
        let add_on_row_id: i64 = r.get("message_add_on_row_id");
        let Some(k) = addon_map.get(&add_on_row_id) else {
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO wa_message_add_on_reaction (chat_jid, key_id, from_me, \
                reaction, sender_timestamp) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&k.chat_jid)
        .bind(&k.key_id)
        .bind(k.from_me)
        .bind(r.get::<Option<String>, _>("reaction"))
        .bind(r.get::<Option<i64>, _>("sender_timestamp"))
        .execute(&mut *tx)
        .await
        .context("insert wa_message_add_on_reaction")?;
        n += 1;
    }
    tx.commit()
        .await
        .context("commit wa_message_add_on_reaction tx")?;
    summary.message_add_on_reaction = n;
    Ok(())
}

/// Walk `media_root`, register every regular file in `wa_media_files`,
/// and put its bytes into the sibling blob_cas keyed by blake3. Translate
/// resolves attachments by joining `wa_message_media.file_path` →
/// `wa_media_files.sha256` → `wa_media_files.blake3` → `cas_objects.bytes`.
///
/// Skips dot-prefixed dirs (`.Thumbs`, `.Shared`, `.trash`, `.wamocache`,
/// …) — those are local WhatsApp scratch state, not message media.
async fn mirror_media_files(
    dst: &SqlitePool,
    target_db_path: &Path,
    media_root: &Path,
    summary: &mut IngestSummary,
) -> Result<()> {
    let media_root_owned = media_root.to_path_buf();
    // Hashing + reading bytes are CPU+IO work — do them on the blocking
    // pool so we don't starve the sqlx async runtime.
    let entries: Vec<MediaEntry> =
        tokio::task::spawn_blocking(move || scan_media(&media_root_owned)).await??;

    // Precompute blake3 for every file; cheap relative to the disk read
    // we already paid for, and lets us batch the CAS insert + the
    // metadata UPDATE.
    let blake3s: Vec<String> = entries.iter().map(|e| blake3_hex(&e.content)).collect();

    // Insert metadata rows with blake3 = NULL up front. Drop-and-rebuild
    // truncates this table on every run, so we don't risk clobbering
    // a previously-good blake3.
    let mut tx = dst.begin().await.context("begin wa_media_files tx")?;
    let mut n = 0u64;
    for e in &entries {
        sqlx::query(
            "INSERT OR IGNORE INTO wa_media_files (sha256, relative_path, size_bytes, \
                mtime_unix, mime_type) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&e.sha256)
        .bind(&e.relative_path)
        .bind(e.size_bytes as i64)
        .bind(e.mtime_unix)
        .bind(&e.mime_type)
        .execute(&mut *tx)
        .await
        .context("insert wa_media_files")?;
        n += 1;
    }
    tx.commit().await.context("commit wa_media_files tx")?;

    // Bulk CAS put — one transaction for the whole batch.
    let cas_path = blob_cas::cas_path_for(target_db_path);
    let cas = BlobCas::open(&cas_path)
        .await
        .with_context(|| format!("open blob_cas at {}", cas_path.display()))?;
    let cas_inserts: Vec<CasInsert<'_>> = entries
        .iter()
        .zip(blake3s.iter())
        .map(|(e, b)| CasInsert {
            blake3: b.as_str(),
            bytes: &e.content,
            content_type: e.mime_type.as_deref(),
        })
        .collect();
    cas.put_many(&cas_inserts)
        .await
        .context("blob_cas put_many wa_media_files")?;

    // Stamp blake3 onto each metadata row now that the bytes are
    // durable in the CAS.
    let mut tx = dst
        .begin()
        .await
        .context("begin wa_media_files blake3 tx")?;
    for (e, b) in entries.iter().zip(blake3s.iter()) {
        sqlx::query("UPDATE wa_media_files SET blake3 = ? WHERE sha256 = ?")
            .bind(b)
            .bind(&e.sha256)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("update wa_media_files.blake3 for {}", e.sha256))?;
    }
    tx.commit()
        .await
        .context("commit wa_media_files blake3 tx")?;

    summary.media_files = n;
    Ok(())
}

struct MediaEntry {
    sha256: String,
    relative_path: String,
    size_bytes: u64,
    mtime_unix: Option<i64>,
    mime_type: Option<String>,
    /// Raw file bytes. Kept in memory between scan and the blob_cas
    /// put so a single walk over `Media/` produces both the metadata
    /// rows and the CAS contents in one pass. Released immediately
    /// after the put.
    content: Vec<u8>,
}

fn scan_media(media_root: &Path) -> Result<Vec<MediaEntry>> {
    use walkdir::WalkDir;
    let mut out = Vec::new();
    for entry in WalkDir::new(media_root).into_iter().filter_entry(|e| {
        // Skip dotfile/dotdir branches (`.Thumbs`, `.Shared`, …).
        e.file_name()
            .to_str()
            .map(|n| !n.starts_with('.'))
            .unwrap_or(true)
    }) {
        let entry = entry.context("walk media root")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(media_root) else {
            continue;
        };
        let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let mut h = Sha256::new();
        h.update(&bytes);
        let sha = format!("{:x}", h.finalize());
        let meta = entry.metadata().ok();
        let size_bytes = bytes.len() as u64;
        let mtime_unix = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        let mime_type = mime_from_ext(path);
        out.push(MediaEntry {
            sha256: sha,
            relative_path: rel.to_string_lossy().into_owned(),
            size_bytes,
            mtime_unix,
            content: bytes,
            mime_type,
        });
    }
    Ok(out)
}

fn mime_from_ext(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "png" => "image/png".to_string(),
        "gif" => "image/gif".to_string(),
        "webp" => "image/webp".to_string(),
        "mp4" => "video/mp4".to_string(),
        "mov" => "video/quicktime".to_string(),
        "webm" => "video/webm".to_string(),
        "mp3" => "audio/mpeg".to_string(),
        "ogg" | "opus" => "audio/ogg".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "amr" => "audio/amr".to_string(),
        "pdf" => "application/pdf".to_string(),
        _ => return None,
    })
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// Stamp the doltlite db with one `dolt_commit('-Am', '…')`. Returns
/// true if a commit hash came back (= something changed). No-op + false
/// when the linked libsqlite3 isn't doltlite (e.g. plain `cargo test`
/// against the bundled `libsqlite3-sys` amalgamation) or when the
/// working tree is clean.
async fn commit_if_dirty(pool: &SqlitePool) -> Result<bool> {
    let hash = doltlite_raw::commit_run(pool, "whatsapp ingest").await?;
    match hash {
        Some(h) => {
            tracing::info!(commit_hash = %h, "whatsapp::ingest committed");
            Ok(true)
        }
        None => {
            tracing::info!("whatsapp::ingest: no commit (clean tree or non-doltlite sqlite)");
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end against the developer's real WhatsApp backup.
    /// Marked `#[ignore]` so `cargo test` doesn't fail on CI / on
    /// boxes that don't have the backup or the env var. Run with
    /// `cargo test -p frankweiler-etl-whatsapp -- --ignored \
    ///  --nocapture real_backup`.
    #[tokio::test]
    #[ignore]
    async fn real_backup() {
        let key_hex = std::env::var("WHATSAPP_BACKUP_DECRYPTION_KEY")
            .expect("WHATSAPP_BACKUP_DECRYPTION_KEY env var must be set");
        let root = frankweiler_whatsapp_backup::decode_hex_key(&key_hex).expect("decode hex key");
        let backup_dir =
            std::path::PathBuf::from(std::env::var("WHATSAPP_BACKUP_DIR").unwrap_or_else(|_| {
                let h = std::env::var("HOME").expect("HOME set");
                format!("{h}/backups/WhatsApp")
            }));
        let tmpdir = tempfile::tempdir().expect("tmpdir");
        let target = tmpdir.path().join("wa_raw.doltlite_db");
        let summary = ingest(&backup_dir, &root, &target)
            .await
            .expect("ingest ok");
        // Test-only diagnostic. `disallowed-macros` would forbid this
        // in production code; the integration test is `#[ignore]`'d so
        // it never runs without `--nocapture`, and the user explicitly
        // wants the summary on the terminal when they invoke it.
        #[allow(clippy::disallowed_macros)]
        {
            eprintln!("summary: {summary:?}");
        }
        assert!(summary.jids > 0);
        assert!(summary.chats > 0);
        assert!(summary.messages > 0);
    }
}
