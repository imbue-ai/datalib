//! Reader for `~/Library/Application Support/BeeperTexts/index.db` —
//! the Beeper desktop app's unified per-account cache.
//!
//! ## Why this module shells out to `sqlite3` instead of using sqlx
//!
//! Our workspace links `sqlx` against doltlite (a SQLite fork with
//! extensions to the record format). Beeper Texts' on-disk SQLite
//! files are written by stock SQLite, and doltlite misreads columns
//! that stock SQLite has stored using newer/different type codes —
//! we empirically observed `typeof(accountID)` returning `"integer"`
//! to doltlite while stock SQLite (CLI on the same file) reports
//! `"text"`. Forcing a CAST/snapshot didn't help.
//!
//! We can't easily add `rusqlite` or another `libsqlite3-sys`-linked
//! crate to the binary, because Cargo's `links = "sqlite3"` rule
//! refuses two copies of the native library in one graph.
//!
//! Easiest robust fix: shell out to the system `sqlite3` CLI, which
//! is stock SQLite on macOS, and parse its JSON output. Slower than
//! an in-process query but correct, and the query volume is small
//! enough (a few hundred threads, a few thousand messages) that
//! latency doesn't matter.
//!
//! Despite the "Matrix-flavored" column names (`mx_room_messages`,
//! `eventID`, `roomID`), index.db is the desktop app's
//! bridge-agnostic message store: rows from every Beeper backend
//! (cloud bridges like Slack / Google Chat, local megabridges like
//! Signal) land here in a shared schema. We re-shape that into our
//! `rooms` / `users` / `events` / `blobs` doltlite tables.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::db::{EventRow, RawDb, RoomRow, UserRow};
use super::FetchSummary;

/// `source` tag stamped on every row this module emits. Distinguishes
/// `beeper_index` rows from future `macos_imessage` rows so the same
/// destination doltlite can hold both without UUID collisions.
pub const SOURCE: &str = "beeper_index";

/// Path to the SQLite CLI. Overridable via `BEEPER_SQLITE3` for
/// hermetic builds; defaults to the macOS system binary.
fn sqlite3_bin() -> String {
    std::env::var("BEEPER_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string())
}

/// Maps a configured canonical network name (what the user puts in
/// `sources:`) to the set of `accountID` prefixes index.db uses to
/// tag rows from that network.
fn account_patterns_for(network: &str) -> &'static [&'static str] {
    match network {
        "signal" => &["local-signal"],
        "googlechat" => &["googlechat", "local-googlechat"],
        "slack" => &["slackgo", "local-slack", "slack"],
        "whatsapp" => &["whatsapp", "local-whatsapp"],
        "telegram" => &["telegram", "local-telegram"],
        "discord" => &["discordgo", "local-discord", "discord"],
        "linkedin" => &["linkedin", "local-linkedin"],
        "twitter" => &["twitter", "local-twitter"],
        "instagram" => &["instagramgo", "local-instagram", "instagram"],
        "facebook" => &["facebookgo", "local-facebook", "facebook"],
        "sms" => &["gmessages", "local-gmessages"],
        "imessage" => &["imessage", "local-imessage"],
        _ => &[],
    }
}

fn matches_network(account_id: &str, network: &str) -> bool {
    for pat in account_patterns_for(network) {
        if account_id == *pat
            || account_id.starts_with(&format!("{pat}."))
            || account_id.starts_with(&format!("{pat}_"))
        {
            return true;
        }
    }
    false
}

/// Run a single SQL query via the system `sqlite3` CLI in JSON-output
/// mode and parse the result. SQL is passed via stdin (so we don't
/// have to worry about shell escaping or argv length limits) and the
/// CLI is invoked in immutable read-only mode so a live writer
/// (Beeper Texts) can't be disturbed.
async fn query_json(db_path: &Path, sql: &str) -> Result<Vec<Value>> {
    // We deliberately use the plain path here, NOT a
    // `file:?immutable=1` URI. `immutable=1` tells SQLite to ignore
    // the WAL — convenient for snapshotting, but it silently hides
    // any rows the writer (Beeper Texts) hasn't checkpointed yet.
    // `-readonly` alone gives us a consistent view that includes
    // the WAL without taking a write lock, which is what we want.
    let mut child = Command::new(sqlite3_bin())
        .arg("-json")
        .arg("-readonly")
        .arg(db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn sqlite3")?;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(sql.as_bytes())
            .await
            .context("write SQL to sqlite3 stdin")?;
        // Explicit drop to close the pipe.
    }
    let output = child.wait_with_output().await.context("wait sqlite3")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "sqlite3 failed (exit={:?}): {}",
            output.status.code(),
            stderr.trim()
        );
    }
    let stdout = output.stdout;
    if stdout.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(&stdout)
        .with_context(|| format!("parse sqlite3 -json output ({} bytes)", stdout.len()))?;
    Ok(value.as_array().cloned().unwrap_or_default())
}

/// Top-level extract for everything in index.db that matches the
/// configured networks. Walks threads → rooms → (participants →
/// users, messages → events, reactions → events, attachments →
/// blobs).
pub async fn ingest(
    db_path: &Path,
    dst: &RawDb,
    media_root: &Path,
    networks: &[String],
    download_media: bool,
    summary: &mut FetchSummary,
    progress: &frankweiler_etl::progress::Progress,
) -> Result<()> {
    // ── threads → rooms ──────────────────────────────────────────────
    let thread_rows = query_json(db_path, "SELECT threadID, accountID, thread FROM threads;")
        .await
        .context("query threads")?;

    let mut target_rooms: Vec<(String, String, Value)> = Vec::new();
    for row in &thread_rows {
        let thread_id = row
            .get("threadID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let account_id = row
            .get("accountID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if account_id == "$space" || account_id.is_empty() {
            continue;
        }
        let matched: Option<&String> = networks
            .iter()
            .find(|n| matches_network(&account_id, n.as_str()));
        let Some(network) = matched else {
            debug!(event = "beeper_thread_skip", account_id = %account_id);
            continue;
        };
        // sqlite3 -json gives us `thread` as either a JSON value (if
        // it round-tripped through JSON1) or a string. Handle both.
        let thread_json = match row.get("thread") {
            Some(Value::Object(_)) | Some(Value::Array(_)) => row["thread"].clone(),
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
            _ => Value::Null,
        };
        target_rooms.push((thread_id, network.clone(), thread_json));
    }
    info!(
        event = "beeper_index_threads_matched",
        matched = target_rooms.len(),
        total = thread_rows.len(),
    );
    progress.set_length(Some(target_rooms.len() as u64));

    let mut seen_users: HashSet<String> = HashSet::new();
    for (thread_id, network, thread_json) in &target_rooms {
        let account_id = thread_json
            .pointer("/accountID")
            .and_then(|v| v.as_str())
            .map(String::from);
        let room_row = build_room_row(thread_id, network, account_id.as_deref(), thread_json);
        dst.upsert_room(&room_row).await?;
        summary.rooms += 1;

        ingest_participants(db_path, dst, thread_id, network, &mut seen_users, summary).await?;
        ingest_messages(
            db_path,
            dst,
            media_root,
            thread_id,
            network,
            download_media,
            summary,
        )
        .await?;
        ingest_reactions(db_path, dst, thread_id, network, summary).await?;

        progress.inc(1);
        progress.set_message(&format!(
            "rooms={} events={} users={} blobs={}",
            summary.rooms, summary.events, summary.users, summary.blobs
        ));
    }
    Ok(())
}

fn build_room_row(
    thread_id: &str,
    network: &str,
    account_id: Option<&str>,
    thread_json: &Value,
) -> RoomRow {
    let thread_type = thread_json
        .pointer("/type")
        .and_then(|v| v.as_str())
        .map(String::from);
    let title = thread_json
        .pointer("/title")
        .and_then(|v| v.as_str())
        .map(String::from);
    let description = thread_json
        .pointer("/description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let room_type = thread_json
        .pointer("/extra/bridge/com.beeper.room_type")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(thread_type.clone());
    let is_dm = room_type.as_deref() == Some("dm") || thread_type.as_deref() == Some("single");
    let is_space = thread_json
        .pointer("/extra/roomType")
        .and_then(|v| v.as_str())
        == Some("m.space");
    // Native source IDs: Beeper stamps the upstream network's
    // canonical channel and workspace identifiers under
    // `thread.extra.bridge.channel.*`. Bridge-specific shapes:
    //   * Signal:     channel.id = conversation UUID, fi.mau.receiver = account UUID
    //   * Google Chat:channel.id = "dm:<encoded>"; no separate workspace id
    //   * Slack:      channel.id = "<team>-<channel>", fi.mau.receiver = "<team>-<user>"
    let external_room_id = thread_json
        .pointer("/extra/bridge/channel/id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let external_workspace_id = thread_json
        .pointer("/extra/bridge/channel/fi.mau.receiver")
        .and_then(|v| v.as_str())
        .map(String::from);
    RoomRow {
        source: SOURCE.to_string(),
        network: network.to_string(),
        native_room_id: thread_id.to_string(),
        external_room_id,
        external_workspace_id,
        account_id: account_id.map(String::from),
        room_type,
        title,
        description,
        is_dm,
        is_space,
        payload: thread_json.clone(),
    }
}

/// SQL-escape a single string for inline embedding. Used to scope
/// per-room queries to a specific roomID. Beeper room IDs only ever
/// contain ASCII, but apostrophe-doubling here keeps us safe against
/// any future surprises.
fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

async fn ingest_participants(
    db_path: &Path,
    dst: &RawDb,
    thread_id: &str,
    network: &str,
    seen_users: &mut HashSet<String>,
    summary: &mut FetchSummary,
) -> Result<()> {
    let sql = format!(
        "SELECT account_id, room_id, id, full_name, nickname, img_url, is_self, is_admin
         FROM participants WHERE room_id = {};",
        sql_quote(thread_id)
    );
    let rows = query_json(db_path, &sql).await?;
    for r in &rows {
        let user_id = r
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if user_id.is_empty() || !seen_users.insert(user_id.clone()) {
            continue;
        }
        let full_name = r
            .get("full_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let nickname = r.get("nickname").and_then(|v| v.as_str()).map(String::from);
        let row = UserRow {
            source: SOURCE.to_string(),
            network: Some(network.to_string()),
            native_user_id: user_id,
            display_name: nickname,
            full_name,
            remote_id: None,
            avatar_blob_id: None,
            payload: r.clone(),
        };
        dst.upsert_user(&row).await?;
        summary.users += 1;
    }
    Ok(())
}

async fn ingest_messages(
    db_path: &Path,
    dst: &RawDb,
    media_root: &Path,
    thread_id: &str,
    network: &str,
    download_media: bool,
    summary: &mut FetchSummary,
) -> Result<()> {
    let sql = format!(
        "SELECT eventID, senderContactID, timestamp, type, isDeleted,
                inReplyToID, lastEditionID, text_content, message
         FROM mx_room_messages
         WHERE roomID = {}
         ORDER BY hsOrder, timestamp;",
        sql_quote(thread_id)
    );
    let rows = query_json(db_path, &sql).await?;
    for r in &rows {
        let event_id = r
            .get("eventID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if event_id.is_empty() {
            continue;
        }
        let sender = r
            .get("senderContactID")
            .and_then(|v| v.as_str())
            .map(String::from);
        let timestamp_ms = r.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let event_type = r
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("UNKNOWN")
            .to_string();
        let in_reply_to = r
            .get("inReplyToID")
            .and_then(|v| v.as_str())
            .map(String::from);
        let edit_of = r
            .get("lastEditionID")
            .and_then(|v| v.as_str())
            .map(String::from);
        let text_content = r
            .get("text_content")
            .and_then(|v| v.as_str())
            .map(String::from);
        // `message` is a JSON string column in the row payload.
        let message_json: Value = match r.get("message") {
            Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
            Some(other) => other.clone(),
            None => Value::Null,
        };

        let text = text_content
            .clone()
            .or_else(|| {
                message_json
                    .pointer("/text")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .filter(|s| !s.is_empty());

        let (reaction_emoji, reaction_target) = if event_type == "REACTION" {
            let emoji = message_json
                .pointer("/action/reactionKey")
                .and_then(|v| v.as_str())
                .map(String::from);
            let target = message_json
                .pointer("/extra/partialReactionContent/relatedEventID")
                .and_then(|v| v.as_str())
                .map(String::from);
            (emoji, target)
        } else {
            (None, None)
        };

        let row = EventRow {
            source: SOURCE.to_string(),
            network: network.to_string(),
            native_room_id: thread_id.to_string(),
            native_event_id: event_id.clone(),
            // Beeper Texts doesn't propagate native message ids
            // into index.db (we checked). Future readers (e.g.
            // local megabridge.db) can populate this.
            external_event_id: None,
            sender_native_user_id: sender,
            event_type,
            timestamp_ms,
            text_content: text,
            reply_to_native_event_id: in_reply_to,
            edit_of_native_event_id: edit_of,
            reaction_emoji,
            reaction_target_native_event_id: reaction_target,
            payload: message_json.clone(),
        };
        let event_uuid = dst.upsert_event(&row).await?;
        summary.events += 1;

        if let Some(attachments) = message_json
            .pointer("/attachments")
            .and_then(|v| v.as_array())
        {
            for (i, att) in attachments.iter().enumerate() {
                if let Err(e) = ingest_attachment(
                    dst,
                    media_root,
                    &event_uuid,
                    i,
                    att,
                    download_media,
                    summary,
                )
                .await
                {
                    warn!(
                        event = "beeper_attachment_failed",
                        event_id = %event_id,
                        slot = i,
                        error = %e
                    );
                }
            }
        }
    }
    Ok(())
}

async fn ingest_reactions(
    db_path: &Path,
    dst: &RawDb,
    thread_id: &str,
    network: &str,
    summary: &mut FetchSummary,
) -> Result<()> {
    let sql = format!(
        "SELECT reactionID, eventID, senderID, description, timestamp, isDeleted
         FROM mx_reactions WHERE roomID = {};",
        sql_quote(thread_id)
    );
    let rows = query_json(db_path, &sql).await?;
    for r in &rows {
        let reaction_id = r
            .get("reactionID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if reaction_id.is_empty() {
            continue;
        }
        let target_event = r
            .get("eventID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let sender = r.get("senderID").and_then(|v| v.as_str()).map(String::from);
        let emoji = r
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from);
        let timestamp_ms = r.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
        let row = EventRow {
            source: SOURCE.to_string(),
            network: network.to_string(),
            native_room_id: thread_id.to_string(),
            native_event_id: reaction_id,
            sender_native_user_id: sender,
            event_type: "REACTION".to_string(),
            timestamp_ms,
            reaction_emoji: emoji,
            reaction_target_native_event_id: Some(target_event),
            payload: Value::Null,
            ..EventRow::default()
        };
        dst.upsert_event(&row).await?;
        summary.events += 1;
    }
    Ok(())
}

/// Parse an attachment.id (mxc:// or localmxc://) into the on-disk
/// directory under `media_root` Beeper Texts stores the file at.
fn parse_attachment_id(att_id: &str) -> Option<(&'static str, &str, &str, String)> {
    if let Some(rest) = att_id.strip_prefix("mxc://") {
        let (server, id) = rest.split_once('/')?;
        let id = id.split('?').next().unwrap_or(id);
        Some(("mxc", server, id, server.to_string()))
    } else if let Some(rest) = att_id.strip_prefix("localmxc://") {
        let (server, id) = rest.split_once('/')?;
        let id = id.split('?').next().unwrap_or(id);
        // Local-bridge mxc URIs come from a `<bridge>.localhost`
        // fake homeserver. The desktop app prepends `localhost` when
        // caching, so `localmxc://local-signal/foo` lands at
        // `media/localhostlocal-signal/foo`.
        let dir = format!("localhost{server}");
        Some(("localmxc", server, id, dir))
    } else {
        None
    }
}

async fn ingest_attachment(
    dst: &RawDb,
    media_root: &Path,
    owning_event_uuid: &str,
    slot: usize,
    att: &Value,
    download_media: bool,
    summary: &mut FetchSummary,
) -> Result<()> {
    let att_id = att
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("attachment without `id`"))?;
    let mime = att
        .get("mimeType")
        .and_then(|v| v.as_str())
        .map(String::from);
    let file_name = att
        .get("fileName")
        .and_then(|v| v.as_str())
        .map(String::from);
    let src_url = att
        .get("srcURL")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| att_id.to_string());
    let slot_str = file_name
        .clone()
        .unwrap_or_else(|| format!("attachment_{slot}"));
    let blob_id = format!("{owning_event_uuid}:{slot}");

    let Some((_scheme, _server, media_id, dir_name)) = parse_attachment_id(att_id) else {
        debug!(event = "beeper_attachment_unknown_scheme", id = %att_id);
        return Ok(());
    };
    let path: PathBuf = media_root.join(&dir_name).join(media_id);

    let stub = frankweiler_etl::blob_cas::RefStub {
        ref_id: &blob_id,
        kind: "beeper_media",
        owning_id: owning_event_uuid,
        slot: &slot_str,
        upstream_uuid: Some(att_id),
        upstream_name: file_name.as_deref(),
        source_url: Some(&src_url),
        content_type: mime.as_deref(),
    };

    if !download_media {
        let mut tx = dst.pool().begin().await.context("begin pre_seed_blob tx")?;
        frankweiler_etl::blob_cas::pre_seed_ref(&mut tx, &stub).await?;
        tx.commit().await.context("commit pre_seed_blob tx")?;
        return Ok(());
    }

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            let mut tx = dst
                .pool()
                .begin()
                .await
                .context("begin record_blob_error tx")?;
            frankweiler_etl::blob_cas::record_ref_error(
                &mut tx,
                &blob_id,
                owning_event_uuid,
                &slot_str,
                &format!("media file not found at {}: {e}", path.display()),
            )
            .await?;
            tx.commit().await.context("commit record_blob_error tx")?;
            summary.blob_errors += 1;
            return Ok(());
        }
    };
    frankweiler_etl::blob_cas::store_bytes(dst.pool(), dst.cas(), &stub, &bytes).await?;
    summary.blobs += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_network_signal() {
        assert!(matches_network("local-signal_xyz", "signal"));
        assert!(!matches_network("local-signal_xyz", "googlechat"));
    }

    #[test]
    fn matches_network_googlechat_exact() {
        assert!(matches_network("googlechat", "googlechat"));
        assert!(matches_network("googlechat.foo", "googlechat"));
        assert!(!matches_network("googlechatlikethis", "googlechat"));
    }

    #[test]
    fn matches_network_slack_dot() {
        assert!(matches_network("slackgo.TXY12345-UAB", "slack"));
    }

    #[test]
    fn parse_attachment_mxc() {
        let (scheme, server, id, dir) = parse_attachment_id("mxc://local.beeper.com/abc").unwrap();
        assert_eq!(scheme, "mxc");
        assert_eq!(server, "local.beeper.com");
        assert_eq!(id, "abc");
        assert_eq!(dir, "local.beeper.com");
    }

    #[test]
    fn parse_attachment_localmxc() {
        let (scheme, server, id, dir) =
            parse_attachment_id("localmxc://local-signal/AwAU").unwrap();
        assert_eq!(scheme, "localmxc");
        assert_eq!(server, "local-signal");
        assert_eq!(id, "AwAU");
        assert_eq!(dir, "localhostlocal-signal");
    }

    #[test]
    fn parse_attachment_strips_query_string() {
        let (_, _, id, _) =
            parse_attachment_id("mxc://server/abc?encryptedFileInfoJSON=xyz").unwrap();
        assert_eq!(id, "abc");
    }

    #[test]
    fn sql_quote_escapes_apostrophes() {
        assert_eq!(sql_quote("ab'cd"), "'ab''cd'");
        assert_eq!(sql_quote("plain"), "'plain'");
    }
}
