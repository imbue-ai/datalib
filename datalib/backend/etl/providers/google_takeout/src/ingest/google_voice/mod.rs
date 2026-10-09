//! Google Voice takeout feed.

pub mod parse;
pub mod schema_raw;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_problems::{RunProblem, SkippedRecord};
use datalib_etl::progress::Progress;
use datalib_etl::prune;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fsscan;
use datalib_problems::{Problem, Reason};
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;
use sqlx::{Sqlite, Transaction};

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
    /// Records deleted because no Voice file still holds them.
    pub removed: usize,
    pub files_removed: usize,
    /// Set when files were removed or rewritten but one could not be read,
    /// so nothing was deleted.
    pub held_back: Option<RunProblem>,
}

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    include_spam: bool,
    progress: &Progress,
    found: &RunProblems,
) -> Result<VoiceSummary> {
    // Voice keeps three kinds of file under one subtree, and they
    // already shared one cursor. Each section takes the changed files
    // whose relative path is its own — no walk of its own, no stat.
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    let changes = scan.changes_since(&prev);
    // Records are keyed by what they say, not by file, so only a read of
    // every Voice file says which records left the input.
    let voice_files: Vec<&fsscan::ScannedFile> = scan
        .files
        .iter()
        .filter(|f| fsscan::is_under(&f.rel, "Voice"))
        .collect();
    let read_all = changes.may_have_dropped_records()
        || changes.needs_reading_under("Voice").count() == voice_files.len();
    let changed: Vec<&fsscan::ScannedFile> = if read_all {
        voice_files
    } else {
        changes.needs_reading_under("Voice").collect()
    };
    let under =
        |f: &fsscan::ScannedFile, dir: &str| fsscan::is_under(&f.rel, &format!("Voice/{dir}"));
    // A file that would not read, or one an attachment of which would
    // not, is left unstamped, so the next run reads it again and its row
    // is re-reported until it reads.
    let mut skipped: Vec<SkippedRecord> = Vec::new();
    let mut unread = |f: &fsscan::ScannedFile, reason: Reason, detail: String| {
        skipped.push(SkippedRecord {
            entry: f.rel.clone(),
            problem: Problem::record(reason, &format!("{}: {detail}", f.rel)),
        });
    };
    // Files that would not read at all; their records are unknown.
    let mut failed = 0usize;
    // Files that read, but an attachment of which did not.
    let mut incomplete: Vec<&fsscan::ScannedFile> = Vec::new();

    let mut message_rows: Vec<VoiceMessageRow> = Vec::new();
    let mut bill_rows: Vec<VoiceBillRow> = Vec::new();
    // Each with the ref its bytes went into the accumulator under, which
    // names the key the flush gives them.
    let mut greetings: Vec<(String, VoiceGreetingRow)> = Vec::new();
    let mut done: Vec<&fsscan::ScannedFile> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut n_attachments = 0usize;

    // ── Calls/ (+ optional Spam/) per-record HTML & orphan audio ────
    for f in &changed {
        let folder = if under(f, "Calls") {
            "calls"
        } else if include_spam && under(f, "Spam") {
            "spam"
        } else {
            continue;
        };
        let mut missing: Vec<String> = Vec::new();
        match ingest_record(
            folder,
            &f.path,
            &mut message_rows,
            &mut acc,
            &mut n_attachments,
            &mut missing,
        ) {
            Ok(false) => {}
            Ok(true) if missing.is_empty() => done.push(f),
            Ok(true) => {
                unread(
                    f,
                    Reason::FetchFailed,
                    format!(
                        "{} attachments could not be read; first: {}",
                        missing.len(),
                        missing[0]
                    ),
                );
                incomplete.push(f);
            }
            Err(e) => {
                unread(f, Reason::Undeserializable, e.root_cause().to_string());
                failed += 1;
            }
        }
    }

    // ── Bills.html ──────────────────────────────────────────────────
    if let Some(f) = changed
        .iter()
        .find(|f| f.rel.eq_ignore_ascii_case("Voice/Bills.html"))
    {
        match std::fs::read_to_string(&f.path).map(|html| parse_bills(&html)) {
            // A bills page with no table header is not one this reader
            // knows; a table with no rows is a page of no bills.
            Ok((headers, _)) if headers.is_empty() => {
                unread(
                    f,
                    Reason::Undeserializable,
                    "the page holds no bills table".to_string(),
                );
                failed += 1;
            }
            Ok((headers, rows)) => {
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
                done.push(f);
            }
            Err(e) => {
                unread(f, Reason::FetchFailed, e.to_string());
                failed += 1;
            }
        }
    }

    // ── Greetings/ (blobs) ──────────────────────────────────────────
    for f in changed.iter().filter(|f| under(f, "Greetings")) {
        let path = &f.path;
        let name = file_name(path);
        let greeting_id = ns_id(&format!("voice:greeting:{name}"));
        match std::fs::read(path) {
            Ok(bytes) => {
                acc.add_fetched(&greeting_id, &name, bytes, guess_content_type(path));
                n_attachments += 1;
                let row = VoiceGreetingRow {
                    id_and_payload: WirePayload {
                        id: greeting_id,
                        payload: json!({ "kind": "greeting", "filename": name }).to_string(),
                    },
                    blake3: None,
                };
                greetings.push((name, row));
                done.push(f);
            }
            Err(e) => {
                unread(f, Reason::FetchFailed, e.to_string());
                failed += 1;
            }
        }
    }

    let n_messages = message_rows.len();
    let n_bills = bill_rows.len();
    let n_greetings = greetings.len();
    progress.set_message(&format!(
        "voice: {n_messages} messages / {n_bills} bills / {n_greetings} greetings",
    ));

    // Whether this run may delete what no Voice file holds: only right
    // after reading every one of them. A Voice file there and unread may
    // hold any of them.
    let unread_voice = scan
        .present_unread
        .iter()
        .filter(|rel| fsscan::is_under(rel, "Voice"))
        .count();
    let deletes = read_all
        && changes.walk_errors == 0
        && super::product_exported(scan, "Voice")
        && failed == 0
        && unread_voice == 0;
    // A rewritten file keeps its old stamp on a run that deleted nothing,
    // so the next run still sees it rewritten and reads every file again.
    let rewritten: HashSet<&str> = changes.modified.iter().map(|f| f.rel.as_str()).collect();
    let stamp = |f: &&&fsscan::ScannedFile| deletes || !rewritten.contains(f.rel.as_str());

    // The attachments land before the files that name them are stamped:
    // a flush that fails leaves the files to be read again.
    let blobs_stored = acc.fetched_len();
    let stored = acc
        .flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
            VoiceAttachmentRow {
                id: VoiceAttachmentRow::pk_recipe(owning, ref_id),
                message_id: owning.to_string(),
                ref_name: ref_id.to_string(),
                blake3: blake3.map(str::to_string),
            }
        })
        .await?;
    let greeting_rows: Vec<VoiceGreetingRow> = greetings
        .into_iter()
        .map(|(name, row)| VoiceGreetingRow {
            blake3: stored.get(&name).cloned(),
            ..row
        })
        .collect();

    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin google_voice tx")?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &bill_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &greeting_rows, &now).await?;
    for f in done.iter().filter(stamp) {
        file_checkpoint::record_file(&mut tx, SCOPE, f).await?;
    }
    // An incomplete file is read again next run. Its old stamp would say
    // "rewritten" for good and make every run read every file, so once
    // this run has read them all it goes, and the file reads as new.
    let mut summary = VoiceSummary {
        messages: n_messages,
        bills: n_bills,
        greetings: n_greetings,
        attachments: n_attachments,
        blobs_stored,
        ..VoiceSummary::default()
    };
    // The prune lands with the stamps: a rewritten file stamped without it
    // would not be seen rewritten again, and what it dropped would stay.
    if deletes {
        for f in &incomplete {
            file_checkpoint::forget_file(&mut tx, SCOPE, &f.rel).await?;
        }
        let kept = Kept {
            messages: message_rows
                .iter()
                .map(|r| r.id_and_payload.id.clone())
                .collect(),
            bills: bill_rows
                .iter()
                .map(|r| r.id_and_payload.id.clone())
                .collect(),
            greetings: greeting_rows
                .iter()
                .map(|r| r.id_and_payload.id.clone())
                .collect(),
        };
        summary.removed = prune_unseen(&mut tx, &kept, include_spam).await?;
        let gone = changes.gone();
        summary.files_removed = gone.len();
        for rel in gone {
            file_checkpoint::forget_file(&mut tx, SCOPE, rel).await?;
        }
    } else if failed + unread_voice > 0 && changes.may_have_dropped_records() {
        summary.held_back = Some(fsscan::Scan::deletions_held_back(failed + unread_voice));
    }
    tx.commit().await.context("commit google_voice tx")?;
    found.skipped("google_voice", skipped);
    Ok(summary)
}

/// The ids a full read of the Voice subtree produced, per table.
struct Kept {
    messages: HashSet<String>,
    bills: HashSet<String>,
    greetings: HashSet<String>,
}

/// Delete the records no Voice file holds, with their attachment
/// edges. Messages only in the folders this run read, so turning spam off
/// never deletes it. Only right after reading every file. Returns how many
/// records went.
async fn prune_unseen(
    tx: &mut Transaction<'_, Sqlite>,
    kept: &Kept,
    include_spam: bool,
) -> Result<usize> {
    let folders: &[&str] = if include_spam {
        &["calls", "spam"]
    } else {
        &["calls"]
    };
    let mut owners: Vec<String> = Vec::new();
    for folder in folders {
        owners.extend(
            prune::prune_scope_in_tx(tx, "voice_messages", &[("folder", folder)], &kept.messages)
                .await?,
        );
    }
    owners.extend(prune::prune_scope_in_tx(tx, "voice_greetings", &[], &kept.greetings).await?);
    let bills = prune::prune_scope_in_tx(tx, "voice_bills", &[], &kept.bills).await?;
    prune::delete_owned_in_tx(tx, "voice_attachments", "message_id", &owners).await?;
    Ok(owners.len() + bills.len())
}

/// Parse one `Calls/`/`Spam/` file into message rows + CAS edges.
/// Returns `true` if the file was understood (and should be
/// checkpointed), `false` if skipped (e.g. an attachment blob already
/// owned by a sibling `.html`). An attachment that would not read adds
/// no edge — a message names only the attachments it read — and is said
/// in `missing`.
fn ingest_record(
    folder: &str,
    path: &Path,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
    missing: &mut Vec<String>,
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
                ingest_text_thread(
                    folder,
                    path,
                    &label,
                    &html,
                    rows,
                    acc,
                    n_attachments,
                    missing,
                )?;
                Ok(true)
            }
            Some(tok) if CallKind::from_type_token(tok).is_some() => {
                let kind = CallKind::from_type_token(tok).unwrap();
                ingest_event(
                    folder,
                    path,
                    &label,
                    kind,
                    &html,
                    rows,
                    acc,
                    n_attachments,
                    missing,
                )?;
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
            missing,
        );
        Ok(true)
    }
}

#[allow(clippy::too_many_arguments)]
fn ingest_text_thread(
    folder: &str,
    path: &Path,
    label: &str,
    html: &str,
    rows: &mut Vec<VoiceMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
    missing: &mut Vec<String>,
) -> Result<()> {
    let msgs = parse_chat_log(html);
    // Voice writes a thread file only for a thread with messages, so one
    // holding none is a file this reader could not read.
    if msgs.is_empty() {
        bail!("the thread holds no message");
    }
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
                        acc.add_fetched(&id, &ref_name, bytes, guess_content_type(&blob_path));
                        *n_attachments += 1;
                        attachment_refs.push(ref_name);
                    }
                    Err(e) => missing.push(format!("read {}: {e}", blob_path.display())),
                }
            } else {
                missing.push(format!("{src} is not in the export"));
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
    Ok(())
}

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
    missing: &mut Vec<String>,
) -> Result<()> {
    let ev = parse_haudio(html);
    if ev.published.trim().is_empty() {
        bail!("the call record has no time");
    }
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
                    acc.add_fetched(&id, &ref_name, bytes, guess_content_type(&blob_path));
                    *n_attachments += 1;
                    audio_ref = Some(ref_name);
                }
                Err(e) => missing.push(format!("read {}: {e}", blob_path.display())),
            }
        } else {
            missing.push(format!("{src} is not in the export"));
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
    Ok(())
}

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
    missing: &mut Vec<String>,
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
            acc.add_fetched(&id, &ref_name, bytes, guess_content_type(path));
            *n_attachments += 1;
        }
        Err(e) => missing.push(format!("read {}: {e}", path.display())),
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

fn normalize_ts(raw: &str) -> Option<String> {
    datalib_time::parse_strict(raw)
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
            parse_filename("William Riker - Text - 2364-03-01T17_00_00Z").unwrap();
        assert_eq!(label, "William Riker");
        assert_eq!(ty.as_deref(), Some("Text"));
        assert_eq!(ts, "2364-03-01T17_00_00Z");
    }

    #[test]
    fn parse_filename_empty_label() {
        let (label, ty, _) = parse_filename(" - Missed - 2364-03-06T17_50_34Z").unwrap();
        assert_eq!(label, "");
        assert_eq!(ty.as_deref(), Some("Missed"));
    }

    #[test]
    fn parse_filename_group_conversation_has_no_type() {
        let (label, ty, ts) = parse_filename("Group Conversation - 2364-12-14T14_01_36Z").unwrap();
        assert_eq!(label, "Group Conversation");
        assert_eq!(ty, None);
        assert_eq!(ts, "2364-12-14T14_01_36Z");
    }

    #[test]
    fn derive_channel_keys_on_phone_not_label() {
        // Same number, different filename labels → same channel.
        let (k1, _) = derive_channel("William Riker", &["+12025550102".to_string()]);
        let (k2, _) = derive_channel("+12025550102", &["+12025550102".to_string()]);
        assert_eq!(k1, k2);
        assert_eq!(k1, "+12025550102");
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
        let out = normalize_ts("2364-03-01T09:00:00.742-08:00").unwrap();
        // millis precision, valid RFC3339
        assert!(datalib_time::parse_strict(&out).is_ok());
    }
}
