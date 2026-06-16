//! Raw-store schema for the ChatGPT provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/data_architecture_plan.md`](/docs/dev/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! ChatGPT-specific notes: upstream supplies stable string ids for
//! every entity (no UUIDv5 recipe needed); `GridRow.when_ts` comes
//! from `conversations.update_time`.
//!
//! ## No listing pre-seed
//!
//! Earlier versions of this provider pre-seeded a stub row for every
//! conversation surfaced by the `/backend-api/conversations` listing
//! and only set `payload` once the detail fetch landed. That tri-state
//! row shape (doesn't exist / pre-seeded / fully fetched) didn't fit
//! `WirePayloadRow` and forced a parallel hand-rolled UPSERT path.
//! We've dropped it: writes only happen post-detail-fetch, every write
//! goes through `bulk_upsert_in_tx`. Skip-check on subsequent syncs
//! compares the listing's `update_time` to the stored
//! `conversations.update_time` — but the two endpoints disagree on
//! shape (listing = ISO-8601 string, detail = Unix-epoch float), so the
//! comparison canonicalizes both to whole-second epoch via
//! `extract::update_time_secs` rather than matching the JSON text. See
//! `docs/dev/data_architecture_ingestion.md` §"No-preseed listing flow"
//! for the rationale.
//!
//! ## Row structs and the bulk-upsert path
//!
//! Each wire-payload entity table is declared as a Rust row struct
//! with `#[derive(WirePayloadRow)]` (`MeRow`, `ConversationRow`); the
//! derive emits both the table's DDL and its
//! [`frankweiler_etl::bulk::BulkUpsertable`] impl from the struct's
//! field list, so the schema and the bind code can't drift. The N:M
//! edge table (`ConversationAttachmentRow`) is hand-rolled since it
//! doesn't fit the wire-payload shape. All three go through the
//! generic [`frankweiler_etl::bulk::bulk_upsert_in_tx`] helper for
//! writes — no table-specific bulk SQL anywhere in this provider's
//! code.
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS file managed
//! by [`frankweiler_etl::blob_cas`]. The extract path bulk-writes via
//! [`frankweiler_etl::blob_cas::BlobCas::put_many`] paired with a
//! bulk UPSERT into `chatgpt_attachments`. Translate joins
//! `chatgpt_attachments` → `cas_objects` on `blake3` via
//! [`BlobBundle::load`](frankweiler_etl::blob_cas::BlobBundle::load),
//! one bundle per rendered conversation bucket.

use frankweiler_etl::blob_cas::CasEdgeRow as _;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::{CasEdgeRow, WirePayloadRow};

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["me", "conversations", "chatgpt_attachments"];

/// `me` — the upstream `/backend-api/me` response.
///
/// One row per ChatGPT account. We keep `email` and `name` denormalized
/// for cheap predicate queries; the full response stays in `payload`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "me")]
pub struct MeRow {
    pub id_and_payload: WirePayload,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// `conversations` — one row per ChatGPT conversation id.
///
/// Stores the raw `/backend-api/conversation/{id}` response as
/// received from the live API. **Rows only exist after a successful
/// detail fetch** — no pre-seed stubs.
///
/// Columns:
/// - `id` — upstream conversation id. Primary key.
/// - `title` — denormalized conversation title for cheap listing
///   queries; the payload remains authoritative.
/// - `update_time` — the detail endpoint's `payload.update_time`
///   (a Unix-epoch float), JSON-encoded. The skip-check compares it
///   against the listing endpoint's value, which arrives as an
///   ISO-8601 string, by canonicalizing both to whole-second epoch
///   (`extract::update_time_secs`) rather than matching JSON text. Also
///   the source for `GridRow.when_ts` (translate side).
/// - `payload` — raw upstream conversation JSON (JSONB on disk).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "conversations")]
pub struct ConversationRow {
    pub id_and_payload: WirePayload,
    pub title: Option<String>,
    pub update_time: Option<String>,
}

/// Index on `conversations.update_time` — supports the listing-derived
/// skip-check that asks "has this conversation changed since we last
/// fetched it?" without scanning the full table.
pub const CONVERSATIONS_UPDATE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_update ON conversations(update_time)";

/// `chatgpt_attachments` — N:M edge between one conversation's
/// attachment slot and a `cas_objects` blob. Replaces this provider's
/// use of the shared `blob_refs` table. The per-attachment metadata
/// translate needs (file name, mime type) already lives in
/// `conversations.payload.mapping[*]...`; we only store the
/// (file_id → blake3) mapping here. Universal CAS-edge shape;
/// see [`frankweiler_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "chatgpt_attachments")]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_id: String,
    pub file_id: String,
    pub blake3: Option<String>,
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        MeRow::ddl(),
        ConversationRow::ddl(),
        CONVERSATIONS_UPDATE_INDEX_DDL.to_string(),
    ];
    out.extend(ConversationAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
