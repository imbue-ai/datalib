//! Raw-store schema for the Anthropic (Claude) provider.
//!
//! Declarations-only, proto-flavored.
//!
//! Anthropic-specific notes: upstream supplies stable UUIDs for every
//! entity (no UUIDv5 recipe needed); `GridRow.when_ts` comes from
//! `conversations.updated_at`; `conversations.payload` is the raw
//! `/api/...` response, not the pre-normalized export shape.
//!
//! ## Row structs and the bulk-upsert path
//!
//! `UserRow`, `OrgRow`, `ConversationRow` derive
//! `WirePayloadRow` so the DDL + bulk-upsert plumbing comes from one
//! source. The N:M edge table (`ConversationAttachmentRow`) is
//! hand-rolled. All four go through `bulk_upsert_in_tx`.
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS. The
//! `anthropic_attachments` edge table holds the (file_uuid → blake3)
//! mapping; the renderer's `AnthropicBlobReader` joins it to
//! `cas_objects`. Replaces this provider's writes into the shared
//! `blob_refs`.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["users", "orgs", "conversations", "anthropic_attachments"];

/// `users` — one row per Anthropic user UUID.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "users")]
pub struct UserRow {
    pub id_and_payload: WirePayload,
    pub email: Option<String>,
    pub full_name: Option<String>,
}

/// `orgs` — one row per Anthropic organization UUID.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "orgs")]
pub struct OrgRow {
    pub id_and_payload: WirePayload,
    pub name: Option<String>,
}

/// `conversations` — one row per Anthropic conversation UUID.
///
/// Stores the raw `/api/.../chat_conversations/{uuid}` payload as
/// received. The translate step applies `normalize_to_export_shape`
/// at read time.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "conversations")]
pub struct ConversationRow {
    pub id_and_payload: WirePayload,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub name: Option<String>,
    pub updated_at: Option<String>,
}

pub const CONVERSATIONS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_org ON conversations(org_uuid)";

pub const CONVERSATIONS_UPDATED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(updated_at)";

/// Idempotent migration adding `conversations.org_name`. The
/// `CREATE TABLE IF NOT EXISTS` already declares this column, so on
/// fresh DBs the `ALTER` is a no-op. Kept around for older databases
/// created before `org_name` existed.
pub const MIGRATION_CONVERSATIONS_ADD_ORG_NAME: &str =
    "ALTER TABLE conversations ADD COLUMN org_name TEXT";

/// `anthropic_attachments` — N:M edge between one conversation's
/// attachment slot and a `cas_objects` blob. Replaces this provider's
/// use of the shared `blob_refs` table.
///
/// Columns:
/// - `id` — synthesized PK `"{conversation_uuid}#{file_uuid}"`.
/// - `conversation_uuid` — FK into [`ConversationRow`]. Indexed for
///   the per-conversation load on render, and projected directly by
///   the dolt_diff union as the natural bucket key.
/// - `file_uuid` — upstream Anthropic `file_uuid`. Skip-check key:
///   `(file_uuid, blake3 IS NOT NULL)` means we already have the
///   bytes.
/// - `blake3` — CAS content hash, NULL until the CAS write succeeds.
pub const ANTHROPIC_ATTACHMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS anthropic_attachments (
    id                TEXT PRIMARY KEY,
    conversation_uuid TEXT NOT NULL,
    file_uuid         TEXT NOT NULL,
    blake3            TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

pub const ANTHROPIC_ATTACHMENTS_BY_CONV_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS anthropic_attachments_by_conv \
     ON anthropic_attachments(conversation_uuid)";

pub const ANTHROPIC_ATTACHMENTS_BY_FILE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS anthropic_attachments_by_file \
     ON anthropic_attachments(file_uuid, blake3)";

#[derive(Debug, Clone)]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_uuid: String,
    pub file_uuid: String,
    pub blake3: Option<String>,
}

impl BulkUpsertable for ConversationAttachmentRow {
    const TABLE: &'static str = "anthropic_attachments";
    const TYPED_COLUMNS: &'static [&'static str] = &["conversation_uuid", "file_uuid", "blake3"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.conversation_uuid)
            .bind(&self.file_uuid)
            .bind(self.blake3.as_deref())
    }
}

pub fn attachment_id_recipe(conversation_uuid: &str, file_uuid: &str) -> String {
    format!("{conversation_uuid}#{file_uuid}")
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        UserRow::ddl(),
        OrgRow::ddl(),
        ConversationRow::ddl(),
        CONVERSATIONS_ORG_INDEX_DDL.to_string(),
        CONVERSATIONS_UPDATED_INDEX_DDL.to_string(),
        ANTHROPIC_ATTACHMENTS_DDL.to_string(),
        ANTHROPIC_ATTACHMENTS_BY_CONV_INDEX_DDL.to_string(),
        ANTHROPIC_ATTACHMENTS_BY_FILE_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
