//! "SMS Backup & Restore" export ingester.
//!
//! Walks an export directory for `*.xml` files, sniffs each one's root
//! (`<smses>` vs `<calls>`), and lands every `<sms>` / `<mms>` / `<call>`
//! record as its own `(id, payload)` raw row. MMS attachment bytes
//! (images, audio recordings, …) are decoded from their base64 `data`
//! attribute and stored as content-addressed blobs in the sibling CAS,
//! linked back to the owning message via the `sms_attachments` edge.
//!
//! Idempotent + resumable:
//!
//!   - Every row's PK is a uuidv5 over stable parsed fields (see
//!     [`schema_raw`]), so re-ingesting a fresh (often superset) export
//!     upserts in place rather than duplicating messages.
//!   - A `(size, mtime)` resume cursor (the shared
//!     [`frankweiler_etl::file_checkpoint`] `ingested_files` table)
//!     skips files already ingested unchanged — the standard
//!     export-shaped-source cursor.

pub mod parse;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobCas, CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::control::DownloadControl;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload};
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use tracing::warn;

use self::parse::{CallRecord, MmsRecord, RootKind, SmsRecord};
use self::schema_raw::{
    full_ddl, ns_id, sha8, SmsAttachmentRow, SmsCallRow, SmsMessageRow, CURSOR_SCOPE_PREFIX,
    DATA_TABLES, EDGE_TABLES,
};

pub use frankweiler_etl::doltlite_raw::db_path_for;

const SCOPE: &str = "sms_backup_restore/xml";

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    /// `--reset-and-redownload`. Truncates every entity / edge data
    /// table + bookkeeping sidecar and clears the file cursors. CAS
    /// bytes (`cas_objects`) survive — same convention as every other
    /// provider.
    pub async fn reset(&self) -> Result<()> {
        let all: Vec<&str> = DATA_TABLES
            .iter()
            .chain(EDGE_TABLES.iter())
            .copied()
            .collect();
        dr::truncate_data_tables(&self.pool, &all).await?;
        file_checkpoint::clear_scope_prefix(&self.pool, CURSOR_SCOPE_PREFIX)
            .await
            .context("clear sms_backup_restore file cursors on reset")?;
        Ok(())
    }

    pub async fn load_payloads(&self, table: &str) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, table).await
    }
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (the orchestrator opens it so the post-download
    /// commit hits the same pool).
    pub db: Option<RawDb>,
    /// Root of the user's export (the directory holding `sms-*.xml` /
    /// `calls-*.xml`). A single file path is also accepted.
    pub input_path: PathBuf,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub files: usize,
    pub sms: usize,
    pub mms: usize,
    pub calls: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
    pub parse_errors: usize,
}

/// Run one ingest pass over the export at `input_path`.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }

    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;

    let mut message_rows: Vec<SmsMessageRow> = Vec::new();
    let mut call_rows: Vec<SmsCallRow> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut fps: Vec<FileFingerprint> = Vec::new();
    let mut summary = FetchSummary::default();

    for path in discover_xml(&opts.input_path) {
        let fp = FileFingerprint::of(&path)?;
        if file_checkpoint::should_skip(&stamped, &fp) {
            continue;
        }
        let xml = match std::fs::read_to_string(&path) {
            Ok(x) => x,
            Err(e) => {
                warn!(event = "sms_file_unreadable", path = %path.display(), error = %e);
                summary.parse_errors += 1;
                continue;
            }
        };
        match parse::detect_root(&xml) {
            Some(RootKind::Smses) => match parse::parse_smses(&xml) {
                Ok((smses, mmses)) => {
                    for s in smses {
                        ingest_sms(&s, &mut message_rows);
                        summary.sms += 1;
                    }
                    for m in mmses {
                        ingest_mms(&m, &mut message_rows, &mut acc, &mut summary.attachments);
                        summary.mms += 1;
                    }
                    fps.push(fp);
                    summary.files += 1;
                }
                Err(e) => {
                    warn!(event = "sms_parse_failed", path = %path.display(), error = %e);
                    summary.parse_errors += 1;
                }
            },
            Some(RootKind::Calls) => match parse::parse_calls(&xml) {
                Ok(calls) => {
                    for c in calls {
                        ingest_call(&c, &mut call_rows);
                        summary.calls += 1;
                    }
                    fps.push(fp);
                    summary.files += 1;
                }
                Err(e) => {
                    warn!(event = "sms_calls_parse_failed", path = %path.display(), error = %e);
                    summary.parse_errors += 1;
                }
            },
            None => {
                warn!(event = "sms_unknown_xml", path = %path.display(),
                      "not an <smses>/<calls> export; skipping");
            }
        }
        opts.progress.set_message(&format!(
            "sms_backup_restore: {} sms / {} mms / {} calls ({} files)",
            summary.sms, summary.mms, summary.calls, summary.files,
        ));
    }

    summary.blobs_stored = acc.bundle_mut().cas_inserts().len();

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin sms_backup_restore tx")?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &call_rows, &now).await?;
    for fp in &fps {
        file_checkpoint::record_finished(&mut tx, SCOPE, fp).await?;
    }
    tx.commit().await.context("commit sms_backup_restore tx")?;

    acc.flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
        SmsAttachmentRow {
            id: SmsAttachmentRow::pk_recipe(owning, ref_id),
            message_id: owning.to_string(),
            ref_name: ref_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;

    Ok(summary)
}

/// One `<sms>` → one `sms_messages` row.
fn ingest_sms(s: &SmsRecord, rows: &mut Vec<SmsMessageRow>) {
    let tel = normalize_tel(&s.address);
    let display = conversation_display(&tel, s.contact_name.as_deref());
    let is_me = s.type_ == 2;
    let box_kind = direction(s.type_);
    let when = ms_to_rfc3339(s.date_ms);
    let id = ns_id(&format!(
        "sms:{tel}:{}:{}:{}",
        s.date_ms,
        s.type_,
        sha8(&s.body)
    ));
    let payload = json!({
        "id": id,
        "kind": "sms",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": s.date_ms,
        "box": box_kind,
        "is_me": is_me,
        "address": s.address,
        "body": s.body,
        "date_sent": s.date_sent_ms,
        "readable_date": s.readable_date,
        "contact_name": s.contact_name,
        "attachments": Vec::<String>::new(),
    });
    rows.push(SmsMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        kind: Some("sms".to_string()),
        box_kind: Some(box_kind.to_string()),
    });
}

/// One `<mms>` → one `sms_messages` row + CAS edges for its blobs.
fn ingest_mms(
    m: &MmsRecord,
    rows: &mut Vec<SmsMessageRow>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) {
    let tel = normalize_tel(&m.address);
    let display = conversation_display(&tel, m.contact_name.as_deref());
    let is_me = m.msg_box == 2;
    let box_kind = direction_box(m.msg_box);
    let when = ms_to_rfc3339(m.date_ms);
    // Prefer the carrier message-id, then the transaction id, then a
    // hash of the body text — whichever is the most stable available
    // discriminator for this (address, date) pair.
    let stable = m
        .m_id
        .clone()
        .or_else(|| m.tr_id.clone())
        .unwrap_or_else(|| sha8(&m.text));
    let id = ns_id(&format!("mms:{tel}:{}:{stable}", m.date_ms));

    let mut attachment_refs: Vec<String> = Vec::new();
    for blob in &m.blobs {
        // Prefix with the message id so the app's non-unique part names
        // (image000000.jpg, recording000000.m4a, …) can't collide in a
        // conversation's blob bundle at render time.
        let ref_name = format!("{id}/{}", blob.name);
        acc.add_fetched(
            &id,
            &ref_name,
            blob.bytes.clone(),
            Some(blob.content_type.clone()),
            Some(blob.name.clone()),
        );
        *n_attachments += 1;
        attachment_refs.push(ref_name);
    }

    let payload = json!({
        "id": id,
        "kind": "mms",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": m.date_ms,
        "box": box_kind,
        "is_me": is_me,
        "address": m.address,
        "text": m.text,
        "m_id": m.m_id,
        "tr_id": m.tr_id,
        "date_sent": m.date_sent_ms,
        "readable_date": m.readable_date,
        "contact_name": m.contact_name,
        "attachments": attachment_refs,
    });
    rows.push(SmsMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        kind: Some("mms".to_string()),
        box_kind: Some(box_kind.to_string()),
    });
}

/// One `<call>` → one `sms_calls` row.
fn ingest_call(c: &CallRecord, rows: &mut Vec<SmsCallRow>) {
    let tel = normalize_tel(&c.number);
    let display = conversation_display(&tel, c.contact_name.as_deref());
    let when = ms_to_rfc3339(c.date_ms);
    let call_type = call_type_str(c.type_);
    let id = ns_id(&format!(
        "call:{tel}:{}:{}:{}",
        c.date_ms, c.type_, c.duration_s
    ));
    let payload = json!({
        "id": id,
        "kind": "call",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": c.date_ms,
        "call_type": call_type,
        "number": c.number,
        "duration": c.duration_s,
        "readable_date": c.readable_date,
        "contact_name": c.contact_name,
    });
    rows.push(SmsCallRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        call_type: Some(call_type.to_string()),
    });
}

// ── helpers ─────────────────────────────────────────────────────────

/// Recursively collect every `*.xml` under `root` (or `root` itself if
/// it's a file), sorted for stable ordering.
fn discover_xml(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml")) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Canonicalize a phone number / address to `+` and digits so the same
/// contact keys to one conversation regardless of source formatting.
/// Short codes (e.g. `8880`) and alphanumeric senders pass through
/// digits-only. Empty input → `"unknown"`.
fn normalize_tel(addr: &str) -> String {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    let plus = trimmed.starts_with('+');
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        // Alphanumeric sender id (e.g. "VERIZON"): keep as-is.
        return trimmed.to_string();
    }
    if plus {
        format!("+{digits}")
    } else {
        digits
    }
}

/// Display name for a conversation: a clean contact name when present,
/// else the normalized number.
fn conversation_display(tel: &str, contact_name: Option<&str>) -> String {
    clean_contact(contact_name).unwrap_or_else(|| tel.to_string())
}

/// A usable contact name, or `None` for the app's `(Unknown)` / empty
/// placeholders.
fn clean_contact(contact_name: Option<&str>) -> Option<String> {
    contact_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "(Unknown)" && *s != "null")
        .map(str::to_string)
}

fn direction(sms_type: i64) -> &'static str {
    if sms_type == 2 {
        "sent"
    } else {
        "inbox"
    }
}

fn direction_box(msg_box: i64) -> &'static str {
    if msg_box == 2 {
        "sent"
    } else {
        "inbox"
    }
}

/// SMS Backup & Restore call `type` codes.
fn call_type_str(t: i64) -> &'static str {
    match t {
        1 => "incoming",
        2 => "outgoing",
        3 => "missed",
        4 => "voicemail",
        5 => "rejected",
        6 => "blocked",
        _ => "call",
    }
}

/// Unix-millis epoch → RFC3339 with explicit `+00:00` offset (millis
/// precision). `None` for a non-positive / unparseable stamp.
fn ms_to_rfc3339(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    if ms <= 0 {
        return None;
    }
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tel_keeps_plus_and_digits() {
        assert_eq!(normalize_tel("+1 (647) 449-5789"), "+16474495789");
        assert_eq!(normalize_tel("7786121000"), "7786121000");
        assert_eq!(normalize_tel("8880"), "8880");
        assert_eq!(normalize_tel("VERIZON"), "VERIZON");
        assert_eq!(normalize_tel(""), "unknown");
    }

    #[test]
    fn call_and_sms_with_same_number_share_conversation_key() {
        let mut msgs = Vec::new();
        ingest_sms(
            &SmsRecord {
                address: "+1 (647) 449-5789".into(),
                date_ms: 1778277198761,
                type_: 1,
                body: "hi".into(),
                ..Default::default()
            },
            &mut msgs,
        );
        let mut calls = Vec::new();
        ingest_call(
            &CallRecord {
                number: "+16474495789".into(),
                duration_s: 0,
                date_ms: 1778698683617,
                type_: 3,
                ..Default::default()
            },
            &mut calls,
        );
        assert_eq!(msgs[0].conversation_key, calls[0].conversation_key);
    }

    #[test]
    fn ms_to_rfc3339_has_explicit_offset() {
        let s = ms_to_rfc3339(1781547510000).unwrap();
        assert!(s.ends_with("+00:00"), "explicit offset: {s}");
        assert!(frankweiler_time::parse_strict(&s).is_ok());
        assert_eq!(ms_to_rfc3339(0), None);
    }

    #[test]
    fn clean_contact_drops_placeholders() {
        assert_eq!(clean_contact(Some("(Unknown)")), None);
        assert_eq!(clean_contact(Some("")), None);
        assert_eq!(
            clean_contact(Some("Jean-Luc Picard")).as_deref(),
            Some("Jean-Luc Picard")
        );
    }
}
