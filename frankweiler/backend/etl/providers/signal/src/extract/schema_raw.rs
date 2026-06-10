//! Raw-store schema for the Signal provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Signal-specific notes:
//!
//! - **Payloads are BLOBs, not JSON.** Signal's backup format is a
//!   stream of prost-encoded `Frame` protobuf messages. We store
//!   each entity's frame verbatim as a `BLOB` so `dolt diff` between
//!   two backup imports reflects exactly the bytes that changed
//!   upstream, with no schema-mapping step in extract. The
//!   `jsonb(?)` / `json(payload)` round-trip used by JSON-shaped
//!   providers (anthropic, chatgpt, notion, …) does **not** apply
//!   here.
//!
//! - **No live upstream UUIDs.** Signal's in-backup `recipient_id`
//!   and `chat_id` are `uint64` ids local to that snapshot. We use
//!   them as string PKs since they're stable across re-imports of
//!   the same backup. `chat_items` has no per-item id of its own —
//!   we synthesize a composite PK; see
//!   [`chat_item_id_recipe`].
//!
//! - **`when_ts` is not declared here.** `chat_items.date_sent` is
//!   the closest event-shaped value and is what the translate side
//!   uses as `GridRow.when_ts`. `account`, `recipients`, `chats`
//!   are not event-shaped.
//!
//! - **Backup-file ingestion, not API.** No cursor, no listing pass,
//!   no UPSERT-as-cheap-noop story. The cursor we use is
//!   [`INGESTED_BACKUPS_DDL`]: a Blake3 hash of the snapshot's
//!   three on-disk files. Re-ingesting the same snapshot is a
//!   single-row PK-lookup skip. See plan §P1.12.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["account", "recipients", "chats", "chat_items"];

/// `account` — exactly one row holding the Signal account proto frame.
///
/// Columns:
/// - `id` — always the string literal `'self'`. The PK is a literal
///   rather than a Signal-side id because the backup format only
///   ever carries one account entity per file.
/// - `payload` — raw prost-encoded `Frame::Account` bytes.
pub const ACCOUNT_DDL: &str = "CREATE TABLE IF NOT EXISTS account (
    id TEXT PRIMARY KEY,
    payload BLOB NULL
)";

/// `recipients` — one row per Signal recipient (peer / group).
///
/// Columns:
/// - `id` — the in-backup `recipient_id` (`uint64` upstream),
///   stringified. Primary key. Stable across re-imports of the same
///   backup.
/// - `identifier` — promoted from the payload; either the e164
///   phone number or the ACI hex string. Lets the translate /
///   indexer joins avoid cracking the protobuf payload open.
/// - `display_name` — promoted from the payload for the same reason.
/// - `payload` — raw prost-encoded `Frame::Recipient` bytes.
pub const RECIPIENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS recipients (
    id TEXT PRIMARY KEY,
    identifier TEXT NULL,
    display_name TEXT NULL,
    payload BLOB NULL
)";

/// `chats` — one row per Signal chat (DM or group thread).
///
/// Columns:
/// - `id` — the in-backup `chat_id` (`uint64` upstream),
///   stringified. Primary key.
/// - `recipient_id` — promoted FK into [`RECIPIENTS_DDL`]; joins
///   `chats` to its peer / group without cracking the payload.
/// - `payload` — raw prost-encoded `Frame::Chat` bytes.
pub const CHATS_DDL: &str = "CREATE TABLE IF NOT EXISTS chats (
    id TEXT PRIMARY KEY,
    recipient_id TEXT NOT NULL,
    payload BLOB NULL
)";

/// `chat_items` — one row per Signal message / call / system event
/// inside a chat.
///
/// Signal's wire format does **not** expose a stable per-item id, so
/// the PK is synthesized; see [`chat_item_id_recipe`].
///
/// Columns:
/// - `id` — synthesized composite PK
///   (`"{chat_id}#{author_id}#{date_sent}"`). Primary key.
/// - `chat_id` — promoted FK into [`CHATS_DDL`].
/// - `author_id` — promoted FK into [`RECIPIENTS_DDL`].
/// - `date_sent` — upstream `chat_item.date_sent`, integer Unix-ms.
///   The closest thing this provider has to an event-shaped
///   timestamp; sourced into `GridRow.when_ts` by translate.
/// - `payload` — raw prost-encoded `Frame::ChatItem` bytes.
pub const CHAT_ITEMS_DDL: &str = "CREATE TABLE IF NOT EXISTS chat_items (
    id TEXT PRIMARY KEY,
    chat_id TEXT NOT NULL,
    author_id TEXT NOT NULL,
    date_sent INTEGER NOT NULL,
    payload BLOB NULL
)";

/// Index on `chats.recipient_id` — supports joining a chat to its
/// peer / group without scanning.
pub const CHATS_BY_RECIPIENT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chats_by_recipient ON chats(recipient_id)";

/// Index on `chat_items(chat_id, date_sent)` — supports the
/// "all messages in a chat, ordered by time" query that translate
/// uses to materialize one document per chat.
pub const CHAT_ITEMS_BY_CHAT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_items_by_chat ON chat_items(chat_id, date_sent)";

/// `ingested_backups` — Signal's resume cursor. One row per Signal
/// snapshot we have already processed.
///
/// **Why this exists.** Signal extracts run against a backup-file
/// snapshot directory (`metadata` + `main` + `files`). Walking and
/// upserting every frame in `main` is idempotent (UPSERT dedup), but
/// still does the work of decrypting and decoding tens of MB of
/// protobuf for nothing. This cursor lets fetch short-circuit at
/// "have we ever ingested this snapshot?" before any of that work.
///
/// **PK choice.** Blake3 hash, hex-encoded, of
/// `metadata || main || files` concatenated in that order. Cheap to
/// compute (single Blake3 stream, three reads), and the recipe lives
/// in [`snapshot_blake3_recipe_doc`] for the writer + reader to
/// agree on.
///
/// **Lifecycle.**
/// - `extract` reads the three files, hashes them, looks up the
///   resulting blake3. If present → skip. If absent → process and
///   then `INSERT` a new row.
/// - `--reset-and-redownload` wipes this table along with the
///   entity tables, so an explicit reset will re-process even a
///   previously-ingested snapshot.
///
/// Columns:
/// - `blake3` — hex-encoded Blake3 of the snapshot's three files.
///   Primary key.
/// - `snapshot_dir` — directory the snapshot was read from, recorded
///   so a user can correlate cursor rows with on-disk locations.
///   Informational only — not part of the skip-check.
/// - `total_byte_size` — combined byte size of `metadata + main +
///   files`. Informational only — a cheap sanity check.
/// - `ingested_at` — ISO-8601 UTC stamp of when we recorded
///   ingestion. NOT NULL: an `ingested_backups` row only exists once
///   ingestion has finished successfully.
pub const INGESTED_BACKUPS_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_backups (
    blake3 TEXT PRIMARY KEY,
    snapshot_dir TEXT NULL,
    total_byte_size INTEGER NULL,
    ingested_at TEXT NOT NULL
)";

/// Documentation-only: the recipe for [`INGESTED_BACKUPS_DDL`]'s PK.
/// Kept as a const string rather than a function because the actual
/// hashing happens in `extract/mod.rs` with streaming I/O — the
/// recipe is a one-line invariant rather than a callable helper.
///
/// Format: `blake3.hex(metadata || main || files)`, where the three
/// names refer to the on-disk files under a Signal snapshot
/// directory and `||` is byte-concatenation in that fixed order.
pub const SNAPSHOT_BLAKE3_RECIPE_DOC: &str =
    "blake3.hex(snapshot_dir/metadata || snapshot_dir/main || snapshot_dir/files)";

/// Recipe for the synthesized [`CHAT_ITEMS_DDL`] primary key.
///
/// Signal's backup format does not carry a per-item id. We hand-roll
/// a composite PK from `(chat_id, author_id, date_sent)` — the only
/// triple guaranteed unique within a single backup. Format is
/// `"{chat_id}#{author_id}#{date_sent}"`.
///
/// This is Signal's analogue of the UUIDv5 recipes other providers
/// document under their (eventual, plan §P0.4) `uuid.rs` modules.
/// For now we keep the recipe **here** with the schema it keys into,
/// so that "what does the PK mean?" is one rustdoc-hop from the DDL.
/// When P0.4 lands we'll decide whether to relocate this recipe into
/// a sibling `uuid.rs` or leave it inline.
pub fn chat_item_id_recipe(chat_id: &str, author_id: &str, date_sent: i64) -> String {
    format!("{chat_id}#{author_id}#{date_sent}")
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        ACCOUNT_DDL.to_string(),
        RECIPIENTS_DDL.to_string(),
        CHATS_DDL.to_string(),
        CHATS_BY_RECIPIENT_INDEX_DDL.to_string(),
        CHAT_ITEMS_DDL.to_string(),
        CHAT_ITEMS_BY_CHAT_INDEX_DDL.to_string(),
        // Resume cursor — see INGESTED_BACKUPS_DDL. Not in
        // DATA_TABLES because it has no upstream-id / bookkeeping
        // shape; reset() truncates it explicitly.
        INGESTED_BACKUPS_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
