//! Raw-store schema for the ChatGPT provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
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
/// render needs (file name, mime type) already lives in
/// `conversations.payload.mapping[*]...`; we only store the
/// (file_id → blake3) mapping here. Universal CAS-edge shape;
/// see [`datalib_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "chatgpt_attachments")]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_id: String,
    pub file_id: String,
    pub blake3: Option<String>,
}

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
