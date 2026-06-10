//! WhatsApp-Android `msgstore.db.crypt15` ingest.
//!
//! The flow is *drop-and-rebuild*: every ingest decrypts the base
//! msgstore.db.crypt15 to a temp file, opens it as a read-only SQLite
//! source, drops the `wa_*` tables in the target doltlite raw store,
//! recreates them with stable PKs (so re-ingests reflect content
//! diffs only, not autoincrement-id churn), and re-inserts every row.
//!
//! Stable-PK rekey rules:
//!
//! * `jid._id` → `raw_string` (e.g. `1234567890@s.whatsapp.net`).
//! * `chat._id` → the chat's jid (resolved via `chat.jid_row_id` →
//!   `jid.raw_string`).
//! * `message._id` → `(chat_jid, key_id, from_me)`. `key_id` is
//!   WhatsApp's wire identifier — stable across phones / backups for
//!   a given real message; the triple disambiguates the case where
//!   `from_me=1` echoes back the same `key_id` as `from_me=0` from
//!   another participant.
//! * `message_add_on._id` → the add-on's own `(chat_jid, key_id,
//!   from_me)` triple. add_ons (reactions, polls, …) carry their
//!   own `key_id`. Parent message is pinned by storing the parent's
//!   triple alongside.
//!
//! Plaintext media files in `~/backups/WhatsApp/Media/` (and
//! sibling directories) are registered in `wa_media_files` keyed
//! by sha256 of file content. The media-file rows reference the
//! relative file path; `wa_message_media.file_path` joins to those.
//!
//! Scope (first pass): curated subset of msgstore tables —
//! `message`, `message_text`, `message_media`, `chat`, `jid`,
//! `message_add_on`, `message_add_on_reaction`, plus the
//! `wa_media_files` registry. msgstore has 200+ tables; the rest
//! (mentions, vcards, locations, quotes, deleted-message records,
//! …) can land here additively when needed without changing the
//! architecture.

pub mod extract;
pub mod schema_raw;
