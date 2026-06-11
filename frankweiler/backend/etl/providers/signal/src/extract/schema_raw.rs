//! Raw-store schema for the Signal provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! ## Five object tables
//!
//! All keyed by Signal's natural ids so re-fetches across snapshots
//! dedupe cleanly:
//!
//!   * `account`               — one row, `id = 'self'`. The
//!     `Frame::Account` JSON payload.
//!   * `recipients`            — PK = the in-backup `recipient_id`
//!     (`uint64`). Promoted columns: `identifier` (e164 / aci hex),
//!     `display_name`.
//!   * `chats`                 — PK = `chat_id`. `recipient_id`
//!     promoted for joins.
//!   * `chat_items`            — PK =
//!     `"{chat_id}#{author_id}#{date_sent}"`. Promoted columns let
//!     SQL queries filter/sort without cracking the JSON payload
//!     open.
//!   * `chat_item_attachments` — N:M edge between a `chat_items`
//!     attachment slot and a `cas_objects` blob. PK =
//!     `"{chat_item_id}#{slot}"`; columns: `chat_item_id` (FK + index),
//!     `ref_id` (Signal's `media_name`), `blake3` (NULL until CAS
//!     write succeeds). Replaces this provider's use of the shared
//!     `blob_refs` table — see [issue
//!     #36](https://github.com/imbue-ai/mixed_up_files/issues/36)
//!     for the design.
//!
//! ## Row structs and the bulk-upsert path
//!
//! Each entity table has a Rust row struct declared right after its
//! `_ddl()` function (`AccountRow`, `RecipientRow`, `ChatRow`,
//! `ChatItemRow`, `ChatItemAttachmentRow`). Each impls
//! [`frankweiler_etl::bulk::BulkUpsertable`], so the generic
//! [`frankweiler_etl::bulk::bulk_upsert_in_tx`] helper writes them
//! through the same chunked multi-row UPSERT. No table-specific bulk
//! SQL anywhere in this provider's code.
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS file managed
//! by [`frankweiler_etl::blob_cas`]. The extract path bulk-writes via
//! [`frankweiler_etl::blob_cas::BlobCas::put_many`] paired with a
//! bulk UPSERT into `chat_item_attachments`. The render path's
//! `SignalBlobReader` (in [`super::super::translate::parse`]) joins
//! `chat_item_attachments` → `cas_objects` on `blake3`.
//!
//! ## Signal-specific notes:
//!
//! - **Payloads are JSONB**, same convention as every other
//!   provider. Signal's backup format is a stream of prost-encoded
//!   `Frame` protobuf messages; we decode each frame in extract via
//!   the serde derive macros injected on the prost types (see
//!   `tools/prost_toolchain/BUILD.bazel`) and store the resulting
//!   JSON. The decode is lossless: every field upstream sent is
//!   present in the JSON. See
//!   `docs/data_architecture_ingestion.md` §"Wire-fidelity of the
//!   raw store" for the principle. The `jsonb(?)` / `json(payload)`
//!   round-trip used by JSON-shaped providers (anthropic, chatgpt,
//!   notion, …) applies here too.
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

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw as dr;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &[
    "account",
    "recipients",
    "chats",
    "chat_items",
    "chat_item_attachments",
];

/// `account` — exactly one row holding the Signal account proto frame.
///
/// Columns:
/// - `id` — always the string literal `'self'`. The PK is a literal
///   rather than a Signal-side id because the backup format only
///   ever carries one account entity per file.
/// - `payload_blake3` — blake3 hex of the `payload` bytes. Used by
///   translate's bucket-fingerprint path to decide whether a
///   dependent document needs to re-render without reading the
///   payload itself. See `super::super::translate::parse`.
/// - `payload` — JSONB of the `Frame::Account` message.
pub fn account_ddl() -> String {
    dr::wire_payload_table_ddl("account", &[])
}

/// Row to upsert into [`account_ddl`]. `id` is always the literal
/// `"self"`. The `payload` is the JSON-serialized `Frame::Account`.
/// `payload_blake3` is blake3 hex of the payload bytes — computed
/// once by the extract path right before the bulk upsert.
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: String,
    pub payload_blake3: String,
    pub payload: String,
}

impl BulkUpsertable for AccountRow {
    const TABLE: &'static str = "account";
    const TYPED_COLUMNS: &'static [&'static str] = &["payload_blake3"];

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.payload_blake3)
            .bind(&self.payload)
    }
}

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
/// - `payload_blake3` — blake3 hex of the `payload` bytes. See the
///   [`account_ddl`] doc comment for the bucket-fingerprint
///   rationale.
/// - `payload` — JSONB of the `Frame::Recipient` message.
pub fn recipients_ddl() -> String {
    dr::wire_payload_table_ddl(
        "recipients",
        &["identifier     TEXT NULL", "display_name   TEXT NULL"],
    )
}

/// Row to upsert into [`recipients_ddl`].
#[derive(Debug, Clone)]
pub struct RecipientRow {
    pub id: String,
    pub identifier: Option<String>,
    pub display_name: Option<String>,
    pub payload_blake3: String,
    pub payload: String,
}

impl BulkUpsertable for RecipientRow {
    const TABLE: &'static str = "recipients";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["identifier", "display_name", "payload_blake3"];

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(self.identifier.as_deref())
            .bind(self.display_name.as_deref())
            .bind(&self.payload_blake3)
            .bind(&self.payload)
    }
}

/// `chats` — one row per Signal chat (DM or group thread).
///
/// Columns:
/// - `id` — the in-backup `chat_id` (`uint64` upstream),
///   stringified. Primary key.
/// - `recipient_id` — promoted FK into [`recipients_ddl`]; joins
///   `chats` to its peer / group without cracking the payload.
/// - `payload_blake3` — blake3 hex of the `payload` bytes. See the
///   [`account_ddl`] doc comment for the bucket-fingerprint
///   rationale.
/// - `payload` — JSONB of the `Frame::Chat` message.
pub fn chats_ddl() -> String {
    dr::wire_payload_table_ddl("chats", &["recipient_id   TEXT NOT NULL"])
}

/// Row to upsert into [`chats_ddl`].
#[derive(Debug, Clone)]
pub struct ChatRow {
    pub id: String,
    pub recipient_id: String,
    pub payload_blake3: String,
    pub payload: String,
}

impl BulkUpsertable for ChatRow {
    const TABLE: &'static str = "chats";
    const TYPED_COLUMNS: &'static [&'static str] = &["recipient_id", "payload_blake3"];

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.recipient_id)
            .bind(&self.payload_blake3)
            .bind(&self.payload)
    }
}

/// `chat_items` — one row per Signal message / call / system event
/// inside a chat.
///
/// Signal's wire format does **not** expose a stable per-item id, so
/// the PK is synthesized; see [`chat_item_id_recipe`].
///
/// Columns:
/// - `id` — synthesized composite PK
///   (`"{chat_id}#{author_id}#{date_sent}"`). Primary key.
/// - `chat_id` — promoted FK into [`chats_ddl`].
/// - `author_id` — promoted FK into [`recipients_ddl`].
/// - `date_sent` — upstream `chat_item.date_sent`, integer Unix-ms.
///   The closest thing this provider has to an event-shaped
///   timestamp; sourced into `GridRow.when_ts` by translate.
/// - `payload_blake3` — blake3 hex of the `payload` bytes. The
///   per-row content fingerprint translate aggregates into a
///   per-document bucket fingerprint to decide whether re-rendering
///   is needed.
/// - `payload` — JSONB of the `Frame::ChatItem` message.
pub fn chat_items_ddl() -> String {
    dr::wire_payload_table_ddl(
        "chat_items",
        &[
            "chat_id        TEXT NOT NULL",
            "author_id      TEXT NOT NULL",
            "date_sent      INTEGER NOT NULL",
        ],
    )
}

/// Row to upsert into [`chat_items_ddl`].
#[derive(Debug, Clone)]
pub struct ChatItemRow {
    pub id: String,
    pub chat_id: String,
    pub author_id: String,
    pub date_sent: i64,
    pub payload_blake3: String,
    pub payload: String,
}

impl BulkUpsertable for ChatItemRow {
    const TABLE: &'static str = "chat_items";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["chat_id", "author_id", "date_sent", "payload_blake3"];

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.chat_id)
            .bind(&self.author_id)
            .bind(self.date_sent)
            .bind(&self.payload_blake3)
            .bind(&self.payload)
    }
}

/// Index on `chats.recipient_id` — supports joining a chat to its
/// peer / group without scanning.
pub const CHATS_BY_RECIPIENT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chats_by_recipient ON chats(recipient_id)";

/// Index on `chat_items(chat_id, date_sent)` — supports the
/// "all messages in a chat, ordered by time" query that translate
/// uses to materialize one document per chat.
pub const CHAT_ITEMS_BY_CHAT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_items_by_chat ON chat_items(chat_id, date_sent)";

/// `chat_item_attachments` — one row per attachment slot of a
/// `chat_items` row.
///
/// The minimal "this entity references this CAS blob" edge — four
/// columns. Per-attachment upstream metadata (`content_type`,
/// `file_name`, etc.) is **not stored here**: every field translate
/// needs is already in `chat_items.payload` (the `FilePointer`
/// proto carries it), and `content_type` for the bytes themselves
/// is on `cas_objects`. Storing it here would be unnecessary
/// duplication.
///
/// This replaces Signal's use of the shared `blob_refs` table; see
/// [issue #36](https://github.com/imbue-ai/mixed_up_files/issues/36)
/// and the `docs/data_architecture_ingestion.md` discussion of the
/// "N:M edge as a per-provider attachment table" pattern.
///
/// Columns:
/// - `id` — synthesized PK `"{chat_item_id}#{slot}"`. The same
///   recipe Signal uses for chat_items (composite-key flattened to
///   a string), so the bookkeeping sidecar's uniform
///   `id TEXT PRIMARY KEY` shape holds without change.
/// - `chat_item_id` — FK into [`chat_items_ddl`]. Explicit (not
///   reverse-parsed from the synthesized PK) so render can fetch
///   "every attachment of this chat_item" in one indexed SELECT
///   instead of either scanning the table or parsing PKs. Paired
///   with the `chat_item_attachments_by_chat_item` index below.
/// - `ref_id` — Signal's `media_name` (=
///   `local_media_name(plaintext_hash, local_key)`). The same value
///   the encrypted on-disk attachment file is named by. Skip-check
///   key: a row with `(ref_id = X, blake3 IS NOT NULL)` means "we
///   have already decrypted and stored `media_name X`", and a later
///   re-walk that sees the same media_name in a different chat_item
///   skips redundant decrypt work.
/// - `blake3` — content hash (64-hex Blake3) of the decrypted
///   bytes once they're in the CAS. `NULL` until the CAS write
///   succeeds; a non-NULL value is what the render-side BlobReader
///   joins back to `cas_objects` on.
pub const CHAT_ITEM_ATTACHMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS chat_item_attachments (
    id           TEXT PRIMARY KEY,
    chat_item_id TEXT NOT NULL,
    ref_id       TEXT NOT NULL,
    blake3       TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

/// Index on `chat_item_id` — supports "give me every attachment
/// of this chat_item in one SELECT," which render uses to bulk-load
/// attachments per message.
pub const CHAT_ITEM_ATTACHMENTS_BY_CHAT_ITEM_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_item_attachments_by_chat_item \
     ON chat_item_attachments(chat_item_id)";

/// Index on `(ref_id, blake3)` — supports the skip-check
/// `WHERE ref_id = ? AND blake3 IS NOT NULL`, which both the
/// extract path (don't re-decrypt) and the translate
/// `SignalBlobReader` (resolve ref_id → blake3) use.
pub const CHAT_ITEM_ATTACHMENTS_BY_REF_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_item_attachments_by_ref \
     ON chat_item_attachments(ref_id, blake3)";

/// Row to upsert into [`CHAT_ITEM_ATTACHMENTS_DDL`].
#[derive(Debug, Clone)]
pub struct ChatItemAttachmentRow {
    pub id: String,
    pub chat_item_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

impl BulkUpsertable for ChatItemAttachmentRow {
    const TABLE: &'static str = "chat_item_attachments";
    const TYPED_COLUMNS: &'static [&'static str] = &["chat_item_id", "ref_id", "blake3"];
    // No payload column — this is an N:M edge table that just
    // records (this chat_item's attachment slot, the CAS blob it
    // references via its content hash). All per-attachment upstream
    // metadata lives in `chat_items.payload` (the `FilePointer`
    // proto fields); per-blob metadata like `content_type` lives in
    // `cas_objects`.
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.chat_item_id)
            .bind(&self.ref_id)
            .bind(self.blake3.as_deref())
    }
}

/// Recipe for the synthesized [`CHAT_ITEM_ATTACHMENTS_DDL`] PK.
/// `"{chat_item_id}#{slot}"` — same `#`-delimited shape Signal uses
/// for `chat_items.id`.
pub fn chat_item_attachment_id_recipe(chat_item_id: &str, slot: usize) -> String {
    format!("{chat_item_id}#{slot}")
}

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
/// **PK choice — `fingerprint`.** A composite stat-derived string
/// `"{metadata_mtime_ns}:{metadata_size}:{main_mtime_ns}:{main_size}:
/// {files_mtime_ns}:{files_size}"`. Cheap (three `stat()`s, no
/// I/O on file bodies), and unique enough in practice: Signal's
/// backup writer emits a fresh directory per snapshot, so the
/// triple-(mtime, size) per file changes on every legitimate
/// snapshot. False-positive skips would require both the same
/// mtime and the same byte length across files, which a real
/// backup pipeline doesn't produce.
///
/// **Forensic — `blake3`.** Hex-encoded Blake3 of
/// `metadata || main || files` concatenated in that order. Computed
/// only after the fingerprint skip misses (so a repeat-skip pays
/// zero I/O). Kept as a forensic column so a user inspecting the
/// table later can verify "yes, that was definitely those bytes."
/// See [`SNAPSHOT_BLAKE3_RECIPE_DOC`].
///
/// **Lifecycle.**
/// - `extract` `stat()`s the three files, builds the fingerprint,
///   looks it up. If present → skip immediately (no I/O on file
///   bodies, no crypto, no walk). If absent → compute blake3,
///   decrypt + walk, then `INSERT` a row with both fingerprint and
///   blake3.
/// - `--reset-and-redownload` wipes this table along with the
///   entity tables, so an explicit reset will re-process even a
///   previously-ingested snapshot.
///
/// Columns:
/// - `fingerprint` — composite stat-derived string. Primary key.
/// - `blake3` — hex-encoded Blake3 of the snapshot's three files.
///   Forensic; never read on the hot path.
/// - `snapshot_dir` — directory the snapshot was read from, recorded
///   so a user can correlate cursor rows with on-disk locations.
///   Informational only.
/// - `total_byte_size` — combined byte size of `metadata + main +
///   files`. Informational only.
/// - `ingested_at` — ISO-8601 UTC stamp of when we recorded
///   ingestion. NOT NULL: an `ingested_backups` row only exists once
///   ingestion has finished successfully.
pub const INGESTED_BACKUPS_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_backups (
    fingerprint TEXT PRIMARY KEY,
    blake3 TEXT NOT NULL,
    snapshot_dir TEXT NULL,
    total_byte_size INTEGER NULL,
    ingested_at TEXT NOT NULL
)";

/// Documentation-only: the recipe for [`INGESTED_BACKUPS_DDL`]'s
/// forensic `blake3` column. Kept as a const string rather than a
/// function because the actual hashing happens in `extract/mod.rs`
/// with streaming I/O — the recipe is a one-line invariant rather
/// than a callable helper.
///
/// Format: `blake3.hex(metadata || main || files)`, where the three
/// names refer to the on-disk files under a Signal snapshot
/// directory and `||` is byte-concatenation in that fixed order.
pub const SNAPSHOT_BLAKE3_RECIPE_DOC: &str =
    "blake3.hex(snapshot_dir/metadata || snapshot_dir/main || snapshot_dir/files)";

/// Build the fingerprint string for a snapshot directory: three
/// `(mtime_ns, byte_size)` pairs joined by `:`, in `(metadata, main,
/// files)` order. Used as the [`INGESTED_BACKUPS_DDL`] PK.
///
/// Errors if any of the three files is missing or unreadable; that's
/// the same condition that would later fail the decrypt pass, so
/// failing fast here is correct.
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

/// Recipe for the synthesized [`chat_items_ddl`] primary key.
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
        account_ddl(),
        recipients_ddl(),
        chats_ddl(),
        CHATS_BY_RECIPIENT_INDEX_DDL.to_string(),
        chat_items_ddl(),
        CHAT_ITEMS_BY_CHAT_INDEX_DDL.to_string(),
        CHAT_ITEM_ATTACHMENTS_DDL.to_string(),
        CHAT_ITEM_ATTACHMENTS_BY_CHAT_ITEM_INDEX_DDL.to_string(),
        CHAT_ITEM_ATTACHMENTS_BY_REF_INDEX_DDL.to_string(),
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
