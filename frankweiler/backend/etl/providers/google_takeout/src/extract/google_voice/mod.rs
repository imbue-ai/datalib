//! Google Voice takeout feed.
//!
//! Parses the `Voice/` subtree of a Google Takeout export — `Calls/`
//! (and optionally `Spam/`) per-record HTML, `Bills.html`, and
//! `Greetings/` — into "closer-to-raw" JSON payload rows. Unlike the
//! other feeds we do NOT preserve the upstream bytes: the input is
//! already-rendered HTML, so we parse it and back out the underlying
//! records (timestamps, phone numbers, person identifiers, transcripts,
//! attachment blob refs). See [`parse`] for the HTML shapes and
//! `schema_raw` for the table/identity design.

pub mod parse;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::WirePayload;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::json;
use tracing::warn;

use self::parse::{parse_bills, parse_chat_log, parse_haudio, CallKind, Party};
use self::schema_raw::{
    ns_id, sha8, VoiceAttachmentRow, VoiceBillRow, VoiceGreetingRow, VoiceMessageRow,
};
use super::db::RawDb;

const SCOPE: &str = "google_takeout/google_voice";

#[derive(Debug, Default, Clone)]
pub struct VoiceSummary {
    /// All `voice_messages` rows (texts + call/voicemail events).
    pub messages: usize,
    pub bills: usize,
    pub greetings: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
}

/// Run one Google Voice ingest pass over `<root>/Voice/`.
///
/// Processes `Calls/` always, and `Spam/` when `include_spam` is set;
/// loads `Bills.html` and `Greetings/` regardless. Idempotent: every
/// row's PK is a uuidv5 over stable parsed fields (see
/// [`schema_raw`]), so re-runs upsert in place.
pub async fn ingest(
    db: &RawDb,
    root: &Path,
    include_spam: bool,
    progress: &Progress,
) -> Result<VoiceSummary> {
    let voice_root = root.join("Voice");
    if !voice_root.exists() {
        return Ok(VoiceSummary::default());
    }
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;

    let mut message_rows: Vec<VoiceMessageRow> = Vec::new();
    let mut bill_rows: Vec<VoiceBillRow> = Vec::new();
    let mut greeting_rows: Vec<VoiceGreetingRow> = Vec::new();
    let mut fps: Vec<FileFingerprint> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut n_attachments = 0usize;

    // ── Calls/ (+ optional Spam/) per-record HTML & orphan audio ────
    let mut folders: Vec<(&str, PathBuf)> = vec![("calls", voice_root.join("Calls"))];
    if include_spam {
        folders.push(("spam", voice_root.join("Spam")));
    }
    for (folder, dir) in &folders {
        if !dir.exists() {
            continue;
        }
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let fp = FileFingerprint::of(&path)?;
            if file_checkpoint::should_skip(&stamped, &fp) {
                continue;
            }
            let consumed = ingest_record(
                folder,
                &path,
                &mut message_rows,
                &mut acc,
                &mut n_attachments,
            )
            .unwrap_or_else(|e| {
                warn!(event = "voice_record_failed", path = %path.display(), error = %e);
                false
            });
            if consumed {
                fps.push(fp);
            }
        }
    }

    // ── Bills.html ──────────────────────────────────────────────────
    let bills_path = voice_root.join("Bills.html");
    if bills_path.exists() {
        let fp = FileFingerprint::of(&bills_path)?;
        if !file_checkpoint::should_skip(&stamped, &fp) {
            match std::fs::read_to_string(&bills_path) {
                Ok(html) => {
                    let (headers, rows) = parse_bills(&html);
                    for cells in rows {
                        let key = sha8(&cells.join("\u{1f}"));
                        let payload = json!({
                            "kind": "bill",
                            "headers": headers,
                            "cells": cells,
                        });
                        bill_rows.push(VoiceBillRow {
                            id_and_payload: WirePayload {
                                id: ns_id(&format!("voice:bill:{key}")),
                                payload: payload.to_string(),
                            },
                        });
                    }
                    fps.push(fp);
                }
                Err(e) => warn!(event = "voice_bills_failed", error = %e),
            }
        }
    }

    // ── Greetings/ (blobs) ──────────────────────────────────────────
    let greetings_dir = voice_root.join("Greetings");
    if greetings_dir.exists() {
        for entry in std::fs::read_dir(&greetings_dir)
            .with_context(|| format!("read_dir {}", greetings_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let fp = FileFingerprint::of(&path)?;
            if file_checkpoint::should_skip(&stamped, &fp) {
                continue;
            }
            let name = file_name(&path);
            let greeting_id = ns_id(&format!("voice:greeting:{name}"));
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let blake3 = blake3_hex(&bytes);
                    acc.add_fetched(
                        &greeting_id,
                        &name,
                        bytes,
                        guess_content_type(&path),
                        Some(name.clone()),
                    );
                    n_attachments += 1;
                    greeting_rows.push(VoiceGreetingRow {
                        id_and_payload: WirePayload {
                            id: greeting_id,
                            payload: json!({ "kind": "greeting", "filename": name }).to_string(),
                        },
                        blake3: Some(blake3),
                    });
                    fps.push(fp);
                }
                Err(e) => {
                    warn!(event = "voice_greeting_failed", path = %path.display(), error = %e)
                }
            }
        }
    }

    let n_messages = message_rows.len();
    let n_bills = bill_rows.len();
    let n_greetings = greeting_rows.len();
    progress.set_message(&format!(
        "voice: {n_messages} messages / {n_bills} bills / {n_greetings} greetings",
    ));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin google_voice tx")?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &bill_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &greeting_rows, &now).await?;
    for fp in &fps {
        file_checkpoint::record_finished(&mut tx, SCOPE, fp).await?;
    }
    tx.commit().await.context("commit google_voice tx")?;

    let blobs_stored = acc.bundle_mut().cas_inserts().len();
    acc.flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
        VoiceAttachmentRow {
            id: VoiceAttachmentRow::pk_recipe(owning, ref_id),
            message_id: owning.to_string(),
            ref_name: ref_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;

    Ok(VoiceSummary {
        messages: n_messages,
        bills: n_bills,
        greetings: n_greetings,
        attachments: n_attachments,
        blobs_stored,
    })
}

/// Parse one `Calls/`/`Spam/` file into message rows + CAS edges.
/// Returns `true` if the file was understood (and should be
/// checkpointed), `false` if skipped (e.g. an attachment blob already
/// owned by a sibling `.html`).
fn ingest_record(
    folder: &str,
    path: &Path,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) -> Result<bool> {
    let name = file_name(path);
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(&name);
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let Some((label, type_token, ts_raw)) = parse_filename(stem) else {
        return Ok(false);
    };

    if ext.as_deref() == Some("html") {
        let html =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        match type_token.as_deref() {
            // Text thread / unnamed-type "Group Conversation" → hChatLog.
            Some("Text") | None => {
                ingest_text_thread(folder, path, &label, &html, rows, acc, n_attachments);
                Ok(true)
            }
            Some(tok) if CallKind::from_type_token(tok).is_some() => {
                let kind = CallKind::from_type_token(tok).unwrap();
                ingest_event(folder, path, &label, kind, &html, rows, acc, n_attachments);
                Ok(true)
            }
            _ => Ok(false),
        }
    } else {
        // Non-HTML: an orphan call/voicemail recording whose `.html`
        // index was deleted is a real event; ingest it. Anything else
        // (MMS image/video) is owned by a sibling `.html` — skip.
        let Some(tok) = type_token.as_deref() else {
            return Ok(false);
        };
        let Some(kind) = CallKind::from_type_token(tok) else {
            return Ok(false);
        };
        let sibling_html = path.with_file_name(format!("{stem}.html"));
        if sibling_html.exists() {
            return Ok(false);
        }
        ingest_orphan_audio(
            folder,
            path,
            &label,
            kind,
            &ts_raw,
            rows,
            acc,
            n_attachments,
        );
        Ok(true)
    }
}

/// hChatLog → one `voice_messages` row per parsed message.
fn ingest_text_thread(
    folder: &str,
    path: &Path,
    label: &str,
    html: &str,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) {
    let msgs = parse_chat_log(html);
    // Channel = the distinct non-me parties in this file.
    let tels: Vec<String> = msgs
        .iter()
        .filter(|m| !m.is_me)
        .filter_map(|m| m.sender.tel.clone())
        .collect();
    let (conversation_key, conversation_display) = derive_channel(label, &tels);

    for m in msgs {
        let when = normalize_ts(&m.dt);
        let sender_id = if m.is_me {
            "me".to_string()
        } else {
            party_id(&m.sender)
        };
        let id = ns_id(&format!(
            "voice:msg:{folder}:{conversation_key}:{}:{sender_id}:{}",
            when.as_deref().unwrap_or(&m.dt),
            sha8(&m.body),
        ));
        // Attachments: resolve each img src to a sibling blob.
        let mut attachment_refs: Vec<String> = Vec::new();
        for src in &m.attachments {
            if let Some(blob_path) = resolve_sibling(path, src) {
                let ref_name = file_name(&blob_path);
                match std::fs::read(&blob_path) {
                    Ok(bytes) => {
                        acc.add_fetched(
                            &id,
                            &ref_name,
                            bytes,
                            guess_content_type(&blob_path),
                            Some(ref_name.clone()),
                        );
                        *n_attachments += 1;
                        attachment_refs.push(ref_name);
                    }
                    Err(e) => {
                        warn!(event = "voice_attachment_unreadable", src = %src, error = %e);
                        acc.add_failed(&id, &ref_name, "attachment unreadable");
                    }
                }
            } else {
                warn!(event = "voice_attachment_missing", src = %src);
                acc.add_failed(&id, src, "attachment not found on disk");
            }
        }
        let payload = json!({
            "id": id.clone(),
            "kind": "text",
            "folder": folder,
            "conversation_key": conversation_key,
            "conversation_display": conversation_display,
            "when": when,
            "when_raw": m.dt,
            "sender": { "tel": m.sender.tel, "name": m.sender.name },
            "is_me": m.is_me,
            "body": m.body,
            "attachments": attachment_refs,
        });
        rows.push(VoiceMessageRow {
            id_and_payload: WirePayload {
                id,
                payload: payload.to_string(),
            },
            conversation_key: Some(conversation_key.clone()),
            when_ts: when,
            kind: Some("text".to_string()),
            folder: Some(folder.to_string()),
        });
    }
}

/// haudio (voicemail / call) → one `voice_messages` row.
#[allow(clippy::too_many_arguments)]
fn ingest_event(
    folder: &str,
    path: &Path,
    label: &str,
    kind: CallKind,
    html: &str,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) {
    let ev = parse_haudio(html);
    let tels: Vec<String> = ev.party.tel.iter().cloned().collect();
    let (conversation_key, conversation_display) = derive_channel(label, &tels);
    let when = normalize_ts(&ev.published);
    let id = ns_id(&format!(
        "voice:{}:{folder}:{}:{}",
        kind.as_str(),
        party_id(&ev.party),
        when.as_deref().unwrap_or(&ev.published),
    ));

    let mut audio_ref: Option<String> = None;
    if let Some(src) = &ev.audio_src {
        if let Some(blob_path) = resolve_sibling(path, src) {
            let ref_name = file_name(&blob_path);
            match std::fs::read(&blob_path) {
                Ok(bytes) => {
                    acc.add_fetched(
                        &id,
                        &ref_name,
                        bytes,
                        guess_content_type(&blob_path),
                        Some(ref_name.clone()),
                    );
                    *n_attachments += 1;
                    audio_ref = Some(ref_name);
                }
                Err(e) => {
                    warn!(event = "voice_audio_unreadable", src = %src, error = %e);
                    acc.add_failed(&id, &ref_name, "audio unreadable");
                }
            }
        }
    }
    let payload = json!({
        "id": id.clone(),
        "kind": kind.as_str(),
        "folder": folder,
        "conversation_key": conversation_key,
        "conversation_display": conversation_display,
        "when": when,
        "when_raw": ev.published,
        "party": { "tel": ev.party.tel, "name": ev.party.name },
        "transcript": ev.transcript,
        "duration": ev.duration,
        "audio": audio_ref,
    });
    rows.push(VoiceMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(conversation_key),
        when_ts: when,
        kind: Some(kind.as_str().to_string()),
        folder: Some(folder.to_string()),
    });
}

/// An orphan recording (`<label> - <Type> - <ts>.mp3` with no sibling
/// `.html`) → one `voice_messages` row carrying just the audio blob.
#[allow(clippy::too_many_arguments)]
fn ingest_orphan_audio(
    folder: &str,
    path: &Path,
    label: &str,
    kind: CallKind,
    ts_raw: &str,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) {
    let party = Party {
        tel: label.starts_with('+').then(|| label.to_string()),
        name: (!label.is_empty() && !label.starts_with('+')).then(|| label.to_string()),
    };
    let tels: Vec<String> = party.tel.iter().cloned().collect();
    let (conversation_key, conversation_display) = derive_channel(label, &tels);
    let when = normalize_ts(&ts_raw.replace('_', ":"));
    let when_raw = ts_raw.replace('_', ":");
    let id = ns_id(&format!(
        "voice:{}:{folder}:{}:{}",
        kind.as_str(),
        party_id(&party),
        when.as_deref().unwrap_or(&when_raw),
    ));
    let ref_name = file_name(path);
    match std::fs::read(path) {
        Ok(bytes) => {
            acc.add_fetched(
                &id,
                &ref_name,
                bytes,
                guess_content_type(path),
                Some(ref_name.clone()),
            );
            *n_attachments += 1;
        }
        Err(e) => {
            warn!(event = "voice_orphan_audio_unreadable", path = %path.display(), error = %e);
            acc.add_failed(&id, &ref_name, "audio unreadable");
        }
    }
    let payload = json!({
        "id": id.clone(),
        "kind": kind.as_str(),
        "folder": folder,
        "conversation_key": conversation_key,
        "conversation_display": conversation_display,
        "when": when,
        "when_raw": when_raw,
        "party": { "tel": party.tel, "name": party.name },
        "audio": ref_name,
        "orphan": true,
    });
    rows.push(VoiceMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(conversation_key),
        when_ts: when,
        kind: Some(kind.as_str().to_string()),
        folder: Some(folder.to_string()),
    });
}

// ── helpers ─────────────────────────────────────────────────────────

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// Split a Voice filename stem `<label> - <Type> - <ts>` into its parts.
/// The Type token is the second-to-last ` - `-delimited segment when
/// present; a two-segment `Group Conversation - <ts>` has no Type
/// (returns `None`), and is parsed as an hChatLog. Returns
/// `(label, type_token, ts_raw)`.
fn parse_filename(stem: &str) -> Option<(String, Option<String>, String)> {
    let parts: Vec<&str> = stem.split(" - ").collect();
    match parts.len() {
        0 | 1 => None,
        2 => Some((parts[0].to_string(), None, parts[1].to_string())),
        n => {
            let label = parts[..n - 2].join(" - ");
            Some((
                label,
                Some(parts[n - 2].to_string()),
                parts[n - 1].to_string(),
            ))
        }
    }
}

/// A party's stable identity fragment for PK recipes: phone, else name,
/// else `"?"`.
fn party_id(p: &Party) -> String {
    p.tel
        .clone()
        .or_else(|| p.name.clone())
        .unwrap_or_else(|| "?".to_string())
}

/// Derive `(conversation_key, display)` for a file from its filename
/// label and the distinct non-me phone numbers it references. Keying on
/// the phone number (not the label) merges name-labeled and
/// number-labeled files for the same contact; groups get a stable
/// participant-set hash.
fn derive_channel(label: &str, tels: &[String]) -> (String, String) {
    let mut distinct: Vec<String> = tels.to_vec();
    distinct.sort();
    distinct.dedup();
    match distinct.len() {
        0 => {
            if label.is_empty() {
                ("unknown".to_string(), "Unknown".to_string())
            } else {
                (format!("label:{label}"), label.to_string())
            }
        }
        1 => {
            let key = distinct[0].clone();
            let display = if label.is_empty() {
                key.clone()
            } else {
                label.to_string()
            };
            (key, display)
        }
        n => {
            let key = format!("group:{}", sha8(&distinct.join(",")));
            let display = if label.is_empty() {
                format!("Group ({n} people)")
            } else {
                label.to_string()
            };
            (key, display)
        }
    }
}

/// Canonicalize a parsed RFC3339 timestamp to millis precision, or
/// `None` if it doesn't parse (the raw value is kept in the payload).
fn normalize_ts(raw: &str) -> Option<String> {
    frankweiler_time::parse_strict(raw)
        .ok()
        .map(|t| t.to_rfc3339_millis())
}

/// Resolve an HTML `src`/`href` to a sibling file. Audio `src` carries
/// the full filename (exact match); MMS `img src` drops the extension,
/// so fall back to a stem match.
fn resolve_sibling(html_path: &Path, src: &str) -> Option<PathBuf> {
    let dir = html_path.parent()?;
    let exact = dir.join(src);
    if exact.is_file() {
        return Some(exact);
    }
    // Stem match: a sibling whose name minus its extension equals `src`.
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let n = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let stem = n.rsplit_once('.').map(|(s, _)| s).unwrap_or(n);
        if stem == src {
            return Some(p);
        }
    }
    None
}

fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
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
        "vcf" => "text/vcard",
        _ => return None,
    };
    Some(ct.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filename_three_segments() {
        let (label, ty, ts) =
            parse_filename("Wes Blackwell - Text - 2019-08-01T21_48_33Z").unwrap();
        assert_eq!(label, "Wes Blackwell");
        assert_eq!(ty.as_deref(), Some("Text"));
        assert_eq!(ts, "2019-08-01T21_48_33Z");
    }

    #[test]
    fn parse_filename_empty_label() {
        let (label, ty, _) = parse_filename(" - Missed - 2009-03-06T17_50_34Z").unwrap();
        assert_eq!(label, "");
        assert_eq!(ty.as_deref(), Some("Missed"));
    }

    #[test]
    fn parse_filename_group_conversation_has_no_type() {
        let (label, ty, ts) = parse_filename("Group Conversation - 2025-12-14T14_01_36Z").unwrap();
        assert_eq!(label, "Group Conversation");
        assert_eq!(ty, None);
        assert_eq!(ts, "2025-12-14T14_01_36Z");
    }

    #[test]
    fn derive_channel_keys_on_phone_not_label() {
        // Same number, different filename labels → same channel.
        let (k1, _) = derive_channel("Wes Blackwell", &["+1410".to_string()]);
        let (k2, _) = derive_channel("+1410", &["+1410".to_string()]);
        assert_eq!(k1, k2);
        assert_eq!(k1, "+1410");
    }

    #[test]
    fn derive_channel_group_is_stable_and_order_free() {
        let a = derive_channel("Group Conversation", &["+1".to_string(), "+2".to_string()]);
        let b = derive_channel("Group Conversation", &["+2".to_string(), "+1".to_string()]);
        assert_eq!(a.0, b.0);
        assert!(a.0.starts_with("group:"));
    }

    #[test]
    fn normalize_ts_canonicalizes_offset() {
        let out = normalize_ts("2019-08-01T14:49:00.742-07:00").unwrap();
        // millis precision, valid RFC3339
        assert!(frankweiler_time::parse_strict(&out).is_ok());
    }
}
