//! WhatsApp-Android `msgstore.db.crypt15` ingest.
//!
//! The flow is *drop-and-rebuild* extract followed by a chat-common
//! render: every ingest decrypts the base msgstore.db.crypt15 to a
//! temp file, opens it as a read-only SQLite source, drops the `wa_*`
//! tables in the target doltlite raw store, recreates them with
//! stable PKs, and re-inserts every row. Then [`translate`] reads the
//! same `wa_*` tables back, builds normalized chats, and hands off to
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! Stable-PK rekey rules (see [`extract`]):
//!
//! * `jid._id` → `raw_string` (e.g. `1234567890@s.whatsapp.net`).
//! * `chat._id` → the chat's jid (resolved via `chat.jid_row_id` →
//!   `jid.raw_string`).
//! * `message._id` → `(chat_jid, key_id, from_me)`.
//! * `message_add_on._id` → the add-on's own `(chat_jid, key_id,
//!   from_me)` triple.

pub mod extract;
pub mod render_and_index_md;
pub mod schema_raw;
