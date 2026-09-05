//! Raw-store schema for the Signal provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};
use uuid::Uuid;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[
    "account",
    "recipients",
    "chats",
    "chat_items",
    "chat_item_attachments",
];

/// `account` — exactly one row holding the Signal account proto frame.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "account")]
pub struct AccountRow {
    pub id_and_payload: WirePayload,
}

/// `recipients` — one row per Signal recipient (peer / group).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "recipients")]
pub struct RecipientRow {
    pub id_and_payload: WirePayload,
    pub identifier: Option<String>,
    pub display_name: Option<String>,
}

/// `chats` — one row per Signal chat (DM or group thread).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chats")]
pub struct ChatRow {
    pub id_and_payload: WirePayload,
    pub recipient_id: String,
}

/// `chat_items` — one row per Signal message / call / system event
/// inside a chat.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chat_items")]
pub struct ChatItemRow {
    pub id_and_payload: WirePayload,
    pub chat_id: String,
    pub author_id: String,
    pub date_sent: i64,
}

/// Index on `chats.recipient_id` — supports joining a chat to its
/// peer / group without scanning.
pub const CHATS_BY_RECIPIENT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chats_by_recipient ON chats(recipient_id)";

/// Index on `chat_items(chat_id, date_sent)` — supports the
/// "all messages in a chat, ordered by time" query that render
/// uses to materialize one document per chat.
pub const CHAT_ITEMS_BY_CHAT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_items_by_chat ON chat_items(chat_id, date_sent)";

/// `chat_item_attachments` — N:M edge between one chat_item's
/// attachment slot and a `cas_objects` blob. Universal CAS-edge
/// shape (see [`datalib_etl::blob_cas::CasEdgeRow`]):
/// `id` (synth PK) + `chat_item_id` (owning FK, indexed) +
/// `ref_id` (= Signal `media_name`, indexed for the skip-check)
/// + `blake3` (CAS hash, NULL until decrypt+store succeed).
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "chat_item_attachments")]
pub struct ChatItemAttachmentRow {
    pub id: String,
    pub chat_item_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

/// Signal-specific PK recipe: `"{chat_item_id}#{slot}"`. The trait's
/// default `pk_recipe` doesn't apply here (see the type-level
/// doc-comment).
pub fn chat_item_attachment_id_recipe(chat_item_id: &str, slot: usize) -> String {
    format!("{chat_item_id}#{slot}")
}

/// `ingested_backups` — Signal's resume cursor. One row per Signal
/// snapshot we have already processed.
pub const INGESTED_BACKUPS_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_backups (
    fingerprint TEXT PRIMARY KEY,
    blake3 TEXT NOT NULL,
    snapshot_dir TEXT NULL,
    total_byte_size INTEGER NULL,
    ingested_at TEXT NOT NULL
)";

/// Documentation-only: the recipe for [`INGESTED_BACKUPS_DDL`]'s
/// forensic `blake3` column. Kept as a const string rather than a
/// function because the actual hashing happens in `download/mod.rs`
/// with streaming I/O — the recipe is a one-line invariant rather
/// than a callable helper.
pub const SNAPSHOT_BLAKE3_RECIPE_DOC: &str =
    "blake3.hex(snapshot_dir/metadata || snapshot_dir/main || snapshot_dir/files)";

/// Build the fingerprint string for a snapshot directory: three
/// `(mtime_ns, byte_size)` pairs joined by `:`, in `(metadata, main,
/// files)` order. Used as the [`INGESTED_BACKUPS_DDL`] PK.
pub fn snapshot_fingerprint(snapshot_dir: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context;
    let mut parts = Vec::with_capacity(6);
    for name in ["metadata", "main", "files"] {
        let path = snapshot_dir.join(name);
        let meta = std::fs::metadata(&path)
            .with_context(|| format!("stat {} for fingerprint", path.display()))?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        parts.push(format!("{}:{}", mtime_ns, meta.len()));
    }
    Ok(parts.join(":"))
}

/// Recipe for the synthesized [`ChatItemRow`] primary key.
pub fn chat_item_id_recipe(chat_id: &str, author_id: &str, date_sent: i64) -> String {
    format!("{chat_id}#{author_id}#{date_sent}")
}

/// v5 namespace for every UUID this provider mints. The bytes spell
/// `signal:backup:` to keep it human-recognizable in dumps.
pub const SIGNAL_UUID_NS: Uuid = Uuid::from_bytes([
    0x51, 0x91, 0xa1, 0x00, 0xba, 0xc1, 0x4e, 0x6f, 0x9f, 0x8a, 0x53, 0x16, 0xa1, 0xba, 0xc1, 0x4e,
]);

pub fn signal_chat_uuid(source: &str, chat_id: &str) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:chat:{source}:{chat_id}").as_bytes(),
    )
    .to_string()
}

pub fn signal_recipient_uuid(source: &str, recipient_id: &str) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:recipient:{source}:{recipient_id}").as_bytes(),
    )
    .to_string()
}

pub fn signal_message_uuid(source: &str, chat_id: &str, author_id: &str, date_sent: i64) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:msg:{source}:{chat_id}:{author_id}:{date_sent}").as_bytes(),
    )
    .to_string()
}

pub fn signal_markdown_uuid(chat_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:doc:{chat_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        AccountRow::ddl(),
        RecipientRow::ddl(),
        ChatRow::ddl(),
        CHATS_BY_RECIPIENT_INDEX_DDL.to_string(),
        ChatItemRow::ddl(),
        CHAT_ITEMS_BY_CHAT_INDEX_DDL.to_string(),
        // Resume cursor — see INGESTED_BACKUPS_DDL. Not in
        // DATA_TABLES because it has no upstream-id / bookkeeping
        // shape; reset() truncates it explicitly.
        INGESTED_BACKUPS_DDL.to_string(),
    ];
    out.extend(ChatItemAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
