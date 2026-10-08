//! `Google Chat/` walker.

use datalib_etl::prune;
use datalib_etl_files::fsscan;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::file_checkpoint;
use datalib_problems::{Problem, Reason};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::attachment_path;
use super::db::RawDb;
use super::schema_raw::{ChatAttachmentRow, ChatGroupRow, ChatMessageRow, ChatUserRow};
use super::time as time_parser;
use datalib_etl::doltlite_raw::WirePayload;

const SCOPE: &str = "google_takeout/google_chat";

#[derive(Debug, Default, Clone)]
pub struct ChatSummary {
    pub groups: usize,
    pub users: usize,
    pub messages: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
    /// Users, groups and messages deleted: a gone file's, and messages a
    /// re-read `messages.json` no longer carries.
    pub removed: usize,
    /// Export files that are gone since the last run.
    pub files_removed: usize,
}

/// The name of the directory a scanned file sits in — a Chat user id
/// or group id, which is how the export names them.
fn parent_dir_name(f: &fsscan::ScannedFile) -> Option<String> {
    let name = f
        .path
        .parent()?
        .file_name()
        .and_then(|s| s.to_str())?
        .to_string();
    (!name.is_empty()).then_some(name)
}

fn read_json(f: &fsscan::ScannedFile) -> Result<Value> {
    let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", f.path.display()))
}

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<ChatSummary> {
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    let changes = scan.changes_since(&prev);
    let mut summary = ChatSummary::default();
    // A file that will not read or parse is left unstamped, so the next
    // run reads it again and its row is re-reported until it reads.
    let mut skipped: Vec<SkippedRecord> = Vec::new();
    let mut unread = |f: &fsscan::ScannedFile, e: anyhow::Error| {
        skipped.push(SkippedRecord {
            entry: f.rel.clone(),
            problem: Problem::record(
                Reason::Undeserializable,
                &format!("{}: {}", f.rel, e.root_cause()),
            ),
        });
    };

    // ── Users ──────────────────────────────────────────────────────
    let mut user_rows: Vec<ChatUserRow> = Vec::new();
    let mut user_files: Vec<&fsscan::ScannedFile> = Vec::new();
    for f in changes
        .needs_reading_by_path()
        .filter(|f| fsscan::is_under(&f.rel, "Google Chat/Users"))
    {
        if f.path.file_name().and_then(|s| s.to_str()) != Some("user_info.json") {
            continue;
        }
        let Some(dir_name) = parent_dir_name(f) else {
            continue;
        };
        let payload = match read_json(f) {
            Ok(payload) => payload,
            Err(e) => {
                unread(f, e);
                continue;
            }
        };
        user_rows.push(ChatUserRow {
            id_and_payload: WirePayload {
                id: dir_name,
                payload: payload.to_string(),
            },
        });
        user_files.push(f);
    }

    // ── Groups + messages ──────────────────────────────────────────
    let mut group_rows: Vec<ChatGroupRow> = Vec::new();
    let mut group_files: Vec<&fsscan::ScannedFile> = Vec::new();
    let mut message_rows: Vec<ChatMessageRow> = Vec::new();
    let mut messages_files: Vec<&fsscan::ScannedFile> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut n_attachments: usize = 0;
    // A group's `messages.json` is all of its messages: what a re-read one
    // no longer carries was deleted.
    let mut messages_by_group: HashMap<String, HashSet<String>> = HashMap::new();
    // Groups whose messages this run read, attachments and all.
    let mut reread_groups: HashSet<String> = HashSet::new();

    // A group is a directory holding `group_info.json` and
    // `messages.json`. Both are just changed files in the scan, so
    // dispatch on which one this is rather than walking the tree.
    for f in changes
        .needs_reading_by_path()
        .filter(|f| fsscan::is_under(&f.rel, "Google Chat/Groups"))
    {
        let Some(dir_name) = parent_dir_name(f) else {
            continue;
        };
        let Some(group_dir) = f.path.parent() else {
            continue;
        };
        match f.path.file_name().and_then(|s| s.to_str()) {
            Some("group_info.json") => {
                let payload = match read_json(f) {
                    Ok(payload) => payload,
                    Err(e) => {
                        unread(f, e);
                        continue;
                    }
                };
                group_rows.push(ChatGroupRow {
                    id_and_payload: WirePayload {
                        id: dir_name.clone(),
                        payload: payload.to_string(),
                    },
                });
                group_files.push(f);
            }
            Some("messages.json") => {
                let parsed = match read_json(f) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        unread(f, e);
                        continue;
                    }
                };
                // Only a file that names its messages says which are gone: one
                // with no `messages` array prunes nothing.
                let listed = parsed.get("messages").and_then(|v| v.as_array());
                let rows: Vec<(ChatMessageRow, &Value)> = listed
                    .into_iter()
                    .flatten()
                    .filter_map(|msg| Some((build_message_row(&dir_name, msg)?, msg)))
                    .collect();
                if let Err(e) =
                    super::require_some_read(&f.rel, listed.map_or(0, Vec::len), rows.len())
                {
                    unread(f, e);
                    continue;
                }
                if listed.is_some() {
                    let kept = rows.iter().map(|(r, _)| r.id_and_payload.id.clone());
                    messages_by_group.insert(dir_name.clone(), kept.collect());
                }
                for (row, msg) in rows {
                    for export_name in attached_export_names(msg) {
                        n_attachments += 1;
                        attach(&mut acc, group_dir, &row.id_and_payload.id, export_name);
                    }
                    message_rows.push(row);
                }
                reread_groups.insert(dir_name);
                messages_files.push(f);
            }
            _ => {}
        }
    }

    retry_unfetched_attachments(db, scan, &reread_groups, &mut acc).await?;

    let n_groups = group_rows.len();
    let n_users = user_rows.len();
    let n_messages = message_rows.len();
    progress.set_message(&format!(
        "chat: {n_groups} groups / {n_users} users / {n_messages} messages",
    ));

    // The attachments land before the files that name them are stamped:
    // a flush that fails leaves the files to be read again.
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

    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin google_chat tx")?;
    bulk_upsert_in_tx(&mut tx, &group_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &user_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    for f in user_files
        .iter()
        .chain(group_files.iter())
        .chain(messages_files.iter())
    {
        file_checkpoint::record_file(&mut tx, SCOPE, f).await?;
    }
    // What the re-read files dropped and what the gone files held go in the
    // transaction that stamps the files: a stamp without its prune would
    // not be read again, and what it dropped would stay.
    for (group, kept) in &messages_by_group {
        summary.removed += delete_group_messages(&mut tx, group, kept).await?;
    }

    // A file that is gone takes its user, group or messages with it — unless
    // a file still here is the record of the same one.
    let read: BTreeSet<&str> = user_files
        .iter()
        .chain(group_files.iter())
        .chain(messages_files.iter())
        .map(|f| f.rel.as_str())
        .collect();
    let present: HashSet<ChatFile> = scan
        .files
        .iter()
        .filter_map(|f| chat_file(&f.rel))
        .collect();
    let gone = if super::product_exported(scan, "Google Chat") {
        changes.gone_by_path(&read)
    } else {
        Vec::new()
    };
    for rel in &gone {
        let Some(record) = chat_file(rel).filter(|r| !present.contains(r)) else {
            continue;
        };
        summary.removed += match &record {
            ChatFile::User(id) => {
                prune::delete_owned_in_tx(&mut tx, "chat_users", "id", std::slice::from_ref(id))
                    .await? as usize
            }
            ChatFile::Group(id) => {
                prune::delete_owned_in_tx(&mut tx, "chat_groups", "id", std::slice::from_ref(id))
                    .await? as usize
            }
            ChatFile::Messages(group) => {
                delete_group_messages(&mut tx, group, &HashSet::new()).await?
            }
        };
    }
    summary.files_removed = gone.len();
    for rel in &gone {
        file_checkpoint::forget_file(&mut tx, SCOPE, rel).await?;
    }
    tx.commit().await.context("commit google_chat tx")?;
    found.skipped("google_chat", skipped);

    summary.groups = n_groups;
    summary.users = n_users;
    summary.messages = n_messages;
    summary.attachments = n_attachments;
    summary.blobs_stored = blobs_stored;
    Ok(summary)
}

/// The `export_name` of every file a message names.
fn attached_export_names(msg: &Value) -> impl Iterator<Item = &str> {
    msg.get("attached_files")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|f| f.get("export_name").and_then(|v| v.as_str()))
        .filter(|name| !name.is_empty())
}

/// Google truncates long on-disk names while keeping the full name in the
/// JSON, so the name is resolved by prefix rather than joined (issue #64).
fn attach(acc: &mut CasEdgeAccumulator, group_dir: &Path, owning: &str, export_name: &str) {
    let resolved = match attachment_path::resolve(group_dir, export_name) {
        attachment_path::Resolved::Exact(p) | attachment_path::Resolved::Truncated(p) => p,
        attachment_path::Resolved::Missing => {
            acc.add_skipped(
                owning,
                export_name,
                Reason::NotFound,
                format!("{export_name} is not in the export"),
            );
            return;
        }
    };
    match std::fs::read(&resolved) {
        Ok(bytes) => {
            // Truncation keeps the extension, so the resolved name says
            // what the bytes are.
            let ct = guess_content_type(&resolved);
            acc.add_fetched(
                owning,
                export_name,
                bytes,
                ct,
                Some(export_name.to_string()),
            );
        }
        Err(e) => acc.add_failed(
            owning,
            export_name,
            format!("read {}: {e}", resolved.display()),
        ),
    }
}

/// An attachment that did not read sits under a stamped `messages.json`,
/// so this is the only retry it gets unless its group is re-read.
async fn retry_unfetched_attachments(
    db: &RawDb,
    scan: &fsscan::Scan,
    reread_groups: &HashSet<String>,
    acc: &mut CasEdgeAccumulator,
) -> Result<()> {
    let unfetched: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT a.message_id, a.export_name, m.group_id \
         FROM chat_attachments a JOIN chat_messages m ON m.id = a.message_id \
         WHERE a.blake3 IS NULL",
    )
    .fetch_all(db.pool())
    .await
    .context("list chat attachments with no bytes")?;
    if unfetched.is_empty() {
        return Ok(());
    }
    let group_dirs: HashMap<String, &Path> = scan
        .files
        .iter()
        .filter(|f| fsscan::is_under(&f.rel, "Google Chat/Groups"))
        .filter_map(|f| Some((parent_dir_name(f)?, f.path.parent()?)))
        .collect();
    for (message_id, export_name, group) in unfetched {
        if reread_groups.contains(&group) {
            continue;
        }
        if let Some(dir) = group_dirs.get(&group) {
            attach(acc, dir, &message_id, &export_name);
        }
    }
    Ok(())
}

/// Delete the messages of `group` not in `keep`, with their attachment
/// edges. Returns how many went.
async fn delete_group_messages(
    tx: &mut Transaction<'_, Sqlite>,
    group: &str,
    keep: &HashSet<String>,
) -> Result<usize> {
    let gone = prune::prune_scope_in_tx(tx, "chat_messages", &[("group_id", group)], keep).await?;
    prune::delete_owned_in_tx(tx, "chat_attachments", "message_id", &gone).await?;
    Ok(gone.len())
}

/// What an export file is the record of, by the directory the export
/// names it with.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ChatFile {
    User(String),
    Group(String),
    Messages(String),
}

fn chat_file(rel: &str) -> Option<ChatFile> {
    let mut parts = rel.rsplitn(3, '/');
    let name = parts.next()?;
    let dir = parts.next()?.to_string();
    if fsscan::is_under(rel, "Google Chat/Users") && name == "user_info.json" {
        Some(ChatFile::User(dir))
    } else if fsscan::is_under(rel, "Google Chat/Groups") {
        match name {
            "group_info.json" => Some(ChatFile::Group(dir)),
            "messages.json" => Some(ChatFile::Messages(dir)),
            _ => None,
        }
    } else {
        None
    }
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
