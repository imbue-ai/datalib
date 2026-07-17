//! Raw-store schema for the "SMS Backup & Restore" provider.
//!
//! Declarations-only, mirroring the convention in `google_voice` /
//! `google_takeout`. Row structs derive
//! [`frankweiler_etl_macros::WirePayloadRow`] (entity tables) or
//! [`frankweiler_etl_macros::CasEdgeRow`] (the per-provider CAS edge
//! table), so the DDL + `BulkUpsertable` plumbing comes from the macros
//! and this file is just the schema description.
//!
//! ## Tables
//!
//! Entity (wire-payload) tables — each gets a paired `_bookkeeping`
//! sidecar via the crate's [`full_ddl`] loop:
//!
//!   - `sms_messages` — one row per `<sms>` *and* per `<mms>` record.
//!   - `sms_calls`    — one row per `<call>` record.
//!
//! CAS edge table — maps `(message_id, ref_name) → blake3`:
//!
//!   - `sms_attachments` — MMS image / audio / recording part bytes.
//!
//! ## Identity (idempotent)
//!
//! The app gives us no stable per-record id, so every row's PK is a
//! uuidv5 over a recipe of the most stable fields. Re-ingesting the same
//! (or a superset) export reproduces identical ids, so an
//! `ON CONFLICT(id) DO UPDATE` upsert collapses re-exports to no-op
//! writes rather than duplicating:
//!
//!   - sms:  `sms:{address}:{date_ms}:{type}:{sha8(body)}`
//!   - mms:  `mms:{address}:{date_ms}:{m_id|tr_id|sha8(text)}`
//!   - call: `call:{number}:{date_ms}:{type}:{duration}`
//!
//! Attachment ref names are the part filename *prefixed with the owning
//! message id* (`{message_id}/{partname}`) so the non-unique MMS part
//! names the app emits (`image000000.jpg`, `recording000000.m4a`, …)
//! can't collide within a conversation's blob bundle at render time.

use frankweiler_etl::doltlite_raw::{WirePayload, WirePayloadRow};
use frankweiler_etl_macros::{CasEdgeRow, WirePayloadRow};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Entity tables — each gets a paired `<table>_bookkeeping` sidecar.
pub const DATA_TABLES: &[&str] = &["sms_messages", "sms_calls"];

/// Per-provider CAS edge tables. Wiped by reset alongside
/// [`DATA_TABLES`] (CAS bytes survive).
pub const EDGE_TABLES: &[&str] = &["sms_attachments"];

/// Every file-cursor scope this provider owns; reset wipes them in one
/// go via `clear_scope_prefix`.
pub const CURSOR_SCOPE_PREFIX: &str = "sms_backup_restore/";

/// Per-provider uuidv5 namespace.
fn sms_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"sms-backup-restore.frankweiler")
}

/// uuidv5 of a recipe under this provider's namespace.
pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&sms_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// First 8 hex chars of the sha256 of `s` — a short, stable content tag
/// for identity recipes.
pub fn sha8(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(8);
    for b in digest.iter().take(4) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `sms_messages` — one row per text (`<sms>`) or multimedia (`<mms>`)
/// message. Promoted columns drive the render-side grouping/sorting
/// without cracking the JSON.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "sms_messages")]
pub struct SmsMessageRow {
    pub id_and_payload: WirePayload,
    /// Channel key — the normalized other-party phone number.
    pub conversation_key: Option<String>,
    /// RFC3339 (millis) event time, for sorting + month bucketing.
    pub when_ts: Option<String>,
    /// `sms|mms`.
    pub kind: Option<String>,
    /// `inbox|sent` (direction).
    pub box_kind: Option<String>,
}

/// `sms_calls` — one row per phone call.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "sms_calls")]
pub struct SmsCallRow {
    pub id_and_payload: WirePayload,
    /// Channel key — the normalized other-party phone number; calls and
    /// texts with the same contact share a conversation.
    pub conversation_key: Option<String>,
    /// RFC3339 (millis) event time.
    pub when_ts: Option<String>,
    /// `incoming|outgoing|missed|voicemail|rejected|blocked`.
    pub call_type: Option<String>,
}

/// `sms_attachments` — per-provider CAS edge for MMS part blobs.
/// Owning entity: the message id. Ref: `{message_id}/{partname}`.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "sms_attachments")]
pub struct SmsAttachmentRow {
    pub id: String,
    pub message_id: String,
    pub ref_name: String,
    pub blake3: Option<String>,
}

/// Full DDL list passed to [`frankweiler_etl::doltlite_raw::open`].
/// Composes every entity table + its `_bookkeeping` sidecar, the CAS
/// edge DDL, and the shared `ingested_files` resume-cursor table.
pub fn full_ddl() -> Vec<String> {
    use frankweiler_etl::blob_cas::CasEdgeRow as _;
    use frankweiler_etl::doltlite_raw::bookkeeping_ddl_for;
    let mut out: Vec<String> = vec![
        SmsMessageRow::ddl(),
        SmsCallRow::ddl(),
        frankweiler_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    out.extend(SmsAttachmentRow::all_ddl());
    for table in DATA_TABLES.iter().chain(EDGE_TABLES.iter()) {
        out.push(bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_id_is_stable_and_distinct() {
        let a = ns_id("sms:+1555:1778277131098:1:abcd1234");
        assert_eq!(a, ns_id("sms:+1555:1778277131098:1:abcd1234"));
        assert_eq!(a.len(), 36);
        assert_ne!(a, ns_id("sms:+1555:1778277131099:1:abcd1234"));
    }

    #[test]
    fn sha8_is_8_hex() {
        let h = sha8("Hello personal Thad, this is imbue Thad.");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn full_ddl_covers_every_table() {
        let blob = full_ddl().join("\n");
        for t in DATA_TABLES.iter().chain(EDGE_TABLES.iter()) {
            assert!(blob.contains(t), "missing DDL for {t}");
            assert!(
                blob.contains(&format!("{t}_bookkeeping")),
                "missing bookkeeping DDL for {t}",
            );
        }
        assert!(blob.contains("ingested_files"));
    }
}
