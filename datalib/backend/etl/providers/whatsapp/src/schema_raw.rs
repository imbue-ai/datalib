//! The one table this provider authors, and its identity recipes.
//!
//! Everything else in the raw store is msgstore.db itself, mirrored table
//! for table by `datalib_etl_sqlite_mirror`, so there is no DDL for it
//! here: the schema is whatever the phone's WhatsApp wrote. Two things a
//! reader of that store trips over. `message.text_data` holds the body
//! only for simple text messages; for links, replies and media captions
//! it is null and the body lives in `message_text` / `message_media` /
//! the add-on tables. And every `*_row_id` column is a rowid foreign key
//! into another mirrored table — `chat.jid_row_id -> jid._id`,
//! `message.chat_row_id -> chat._id` — which render resolves to the
//! natural keys the uuids below are minted from.

use uuid::Uuid;

/// Catalog of plaintext media files from the source backup. Bytes live in the
/// sibling CAS, and `blake3` is both this table's key and the CAS key.
///
/// `relative_path` is relative to the **backup root**, so it includes the
/// `Media/` prefix that msgstore puts on `message_media.file_path`. The
/// shipped join is `wa_media_files.relative_path = message_media.file_path`
/// and it only matches because both sides are anchored there.
///
/// Deliberately one digest: that same join is all render needs, so a second
/// hash was stored and never read back — and computing it forced a full
/// re-read of every media file on every run.
pub const WA_MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_media_files (
    blake3 TEXT PRIMARY KEY,
    relative_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mime_type TEXT,
    CHECK (length(blake3) = 64)
);";

pub const WA_MEDIA_FILES: &str = "wa_media_files";

/// All DDL this provider owns. The mirror engine keeps these tables
/// beside the mirrored ones (`MirrorOptions::sidecar_tables`).
pub const ALL_DDL: &[&str] = &[WA_MEDIA_FILES_DDL];

/// v5 namespace for every UUID this provider mints. The bytes spell
/// `whatsapp:msgstr:` to keep it human-recognizable in dumps.
pub const WHATSAPP_UUID_NS: Uuid = Uuid::from_bytes([
    0x77, 0xa7, 0x59, 0xc0, 0xba, 0xc1, 0x4e, 0x6f, 0x9f, 0x8a, 0x73, 0x16, 0xc7, 0xba, 0xc7, 0xc0,
]);

pub fn whatsapp_chat_uuid(source: &str, chat_jid: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:chat:{source}:{chat_jid}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_message_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:msg:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_reaction_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:react:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_markdown_uuid(chat_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:doc:{chat_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}
