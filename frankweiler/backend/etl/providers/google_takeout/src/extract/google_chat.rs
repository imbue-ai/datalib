//! `Google Chat/` walker.
//!
//! Surfaces three on-disk shapes:
//!
//!   - `Google Chat/Groups/<dir>/group_info.json` → `chat_groups` row
//!     keyed by the takeout directory name.
//!   - `Google Chat/Groups/<dir>/messages.json` → one `chat_messages`
//!     row per entry; `group_id` references the parent dir name.
//!   - `Google Chat/Users/User <id>/user_info.json` → `chat_users` row.
//!
//! Attachments referenced by message rows land in `chat_attachments`
//! (the per-provider CAS edge) + `cas_objects` via the shared
//! `CasEdgeAccumulator` / `flush_cas_edges` primitives.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::Value;
use tracing::warn;

use super::attachment_path;
use super::db::RawDb;
use super::schema_raw::{ChatAttachmentRow, ChatGroupRow, ChatMessageRow, ChatUserRow};
use super::time as time_parser;
use frankweiler_etl::doltlite_raw::WirePayload;

const SCOPE: &str = "google_takeout/google_chat";

#[derive(Debug, Default, Clone)]
pub struct ChatSummary {
    pub groups: usize,
    pub users: usize,
    pub messages: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
}

pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<ChatSummary> {
    let chat_root = root.join("Google Chat");
    if !chat_root.exists() {
        return Ok(ChatSummary::default());
    }
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;
    let mut summary = ChatSummary::default();

    // ── Users ──────────────────────────────────────────────────────
    let users_dir = chat_root.join("Users");
    let mut user_rows: Vec<ChatUserRow> = Vec::new();
    let mut user_fps: Vec<FileFingerprint> = Vec::new();
    if users_dir.exists() {
        for entry in std::fs::read_dir(&users_dir)
            .with_context(|| format!("read_dir {}", users_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let info = path.join("user_info.json");
            if !info.exists() {
                continue;
            }
            let fp = FileFingerprint::of(&info)?;
            if file_checkpoint::should_skip(&stamped, &fp) {
                continue;
            }
            let dir_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if dir_name.is_empty() {
                continue;
            }
            let bytes = std::fs::read(&info)?;
            let payload: Value = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", info.display()))?;
            user_rows.push(ChatUserRow {
                id_and_payload: WirePayload {
                    id: dir_name,
                    payload: payload.to_string(),
                },
            });
            user_fps.push(fp);
        }
    }

    // ── Groups + messages ──────────────────────────────────────────
    let groups_dir = chat_root.join("Groups");
    let mut group_rows: Vec<ChatGroupRow> = Vec::new();
    let mut group_fps: Vec<FileFingerprint> = Vec::new();
    let mut message_rows: Vec<ChatMessageRow> = Vec::new();
    let mut messages_fps: Vec<FileFingerprint> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut n_attachments: usize = 0;
    if groups_dir.exists() {
        for entry in std::fs::read_dir(&groups_dir)
            .with_context(|| format!("read_dir {}", groups_dir.display()))?
        {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let dir_name = dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if dir_name.is_empty() {
                continue;
            }
            // group_info.json (small; one row per dir)
            let info = dir.join("group_info.json");
            if info.exists() {
                let fp = FileFingerprint::of(&info)?;
                if !file_checkpoint::should_skip(&stamped, &fp) {
                    let bytes = std::fs::read(&info)?;
                    let payload: Value = serde_json::from_slice(&bytes)
                        .with_context(|| format!("parse {}", info.display()))?;
                    group_rows.push(ChatGroupRow {
                        id_and_payload: WirePayload {
                            id: dir_name.clone(),
                            payload: payload.to_string(),
                        },
                    });
                    group_fps.push(fp);
                }
            }
            // messages.json (one entry per message)
            let messages = dir.join("messages.json");
            if messages.exists() {
                let fp = FileFingerprint::of(&messages)?;
                if !file_checkpoint::should_skip(&stamped, &fp) {
                    let bytes = std::fs::read(&messages)?;
                    let parsed: Value = serde_json::from_slice(&bytes)
                        .with_context(|| format!("parse {}", messages.display()))?;
                    let arr = parsed
                        .get("messages")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    for msg in arr {
                        if let Some(row) = build_message_row(&dir_name, &msg) {
                            // Attachments: walk msg.attached_files[].
                            if let Some(files) =
                                msg.get("attached_files").and_then(|v| v.as_array())
                            {
                                for f in files {
                                    let export_name = f
                                        .get("export_name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    if export_name.is_empty() {
                                        continue;
                                    }
                                    n_attachments += 1;
                                    let owning = row.id_and_payload.id.clone();
                                    // Google truncates long on-disk names
                                    // while keeping the full name in the
                                    // JSON, so resolve via prefix match
                                    // rather than an exact join (issue #64).
                                    let resolved =
                                        match attachment_path::resolve(&dir, &export_name) {
                                            attachment_path::Resolved::Exact(p)
                                            | attachment_path::Resolved::Truncated(p) => p,
                                            attachment_path::Resolved::Missing => {
                                                warn!(
                                                    event = "chat_attachment_missing",
                                                    message_id = %owning,
                                                    export_name = %export_name,
                                                );
                                                acc.add_failed(
                                                    &owning,
                                                    &export_name,
                                                    "attachment file missing on disk",
                                                );
                                                continue;
                                            }
                                        };
                                    match std::fs::read(&resolved) {
                                        Ok(bytes) => {
                                            // Content type from the resolved
                                            // file's extension (truncation
                                            // preserves it).
                                            let ct = guess_content_type(&resolved);
                                            acc.add_fetched(
                                                &owning,
                                                &export_name,
                                                bytes,
                                                ct,
                                                Some(export_name.clone()),
                                            );
                                        }
                                        Err(e) => {
                                            warn!(
                                                event = "chat_attachment_unreadable",
                                                message_id = %owning,
                                                export_name = %export_name,
                                                error = %e,
                                            );
                                            acc.add_failed(
                                                &owning,
                                                &export_name,
                                                "attachment unreadable",
                                            );
                                        }
                                    }
                                }
                            }
                            message_rows.push(row);
                        }
                    }
                    messages_fps.push(fp);
                }
            }
        }
    }

    let n_groups = group_rows.len();
    let n_users = user_rows.len();
    let n_messages = message_rows.len();
    progress.set_message(&format!(
        "chat: {n_groups} groups / {n_users} users / {n_messages} messages",
    ));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin google_chat tx")?;
    bulk_upsert_in_tx(&mut tx, &group_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &user_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    for fp in user_fps
        .iter()
        .chain(group_fps.iter())
        .chain(messages_fps.iter())
    {
        file_checkpoint::record_finished(&mut tx, SCOPE, fp).await?;
    }
    tx.commit().await.context("commit google_chat tx")?;

    // CAS-edge flush: bytes → CAS, then edge rows + bookkeeping in
    // one entity-pool tx.
    let blobs_stored = acc.bundle_mut().cas_inserts().len();
    acc.flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
        ChatAttachmentRow {
            id: ChatAttachmentRow::pk_recipe(owning, ref_id),
            message_id: owning.to_string(),
            export_name: ref_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;

    summary.groups = n_groups;
    summary.users = n_users;
    summary.messages = n_messages;
    summary.attachments = n_attachments;
    summary.blobs_stored = blobs_stored;
    Ok(summary)
}

fn build_message_row(group_id: &str, msg: &Value) -> Option<ChatMessageRow> {
    let message_id = msg.get("message_id").and_then(|v| v.as_str())?.to_string();
    if message_id.is_empty() {
        return None;
    }
    let sender_email = msg
        .get("creator")
        .and_then(|v| v.get("email"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let when_str = msg.get("created_date").and_then(|v| v.as_str());
    let when_ts = when_str.and_then(time_parser::parse_chat_long_form);
    let payload = serde_json::to_string(msg).ok()?;
    Some(ChatMessageRow {
        id_and_payload: WirePayload {
            id: message_id,
            payload,
        },
        group_id: group_id.to_string(),
        sender_email,
        when_ts,
    })
}

fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "mp4" => "video/mp4",
        _ => return None,
    };
    Some(ct.to_string())
}
