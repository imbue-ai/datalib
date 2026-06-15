//! Raw-store schema for the Signal provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/data_architecture_plan.md`](/docs/dev/data_architecture_plan.md)
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
//! Each wire-payload entity table is declared as a Rust row struct
//! with `#[derive(WirePayloadRow)]` (`AccountRow`, `RecipientRow`,
//! `ChatRow`, `ChatItemRow`); the derive generates both the table's
//! DDL and its [`frankweiler_etl::bulk::BulkUpsertable`] impl from the
//! struct's field list, so the schema and the bind code can't drift.
//! The N:M edge table (`ChatItemAttachmentRow`) is hand-rolled since
//! it doesn't fit the wire-payload shape. All four go through the
//! generic [`frankweiler_etl::bulk::bulk_upsert_in_tx`] helper for
//! writes — no table-specific bulk SQL anywhere in this provider's
//! code.
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS file managed
//! by [`frankweiler_etl::blob_cas`]. The extract path bulk-writes via
//! [`frankweiler_etl::blob_cas::BlobCas::put_many`] paired with a
//! bulk UPSERT into `chat_item_attachments`. Translate joins
//! `chat_item_attachments` → `cas_objects` on `blake3` via
//! [`BlobBundle::load`](frankweiler_etl::blob_cas::BlobBundle::load),
//! one bundle per rendered chat bucket.
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
//!   `docs/dev/data_architecture_ingestion.md` §"Wire-fidelity of the
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

use frankweiler_etl::blob_cas::CasEdgeRow as _;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::{CasEdgeRow, WirePayloadRow};

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
/// - `payload` — JSONB of the `Frame::Account` message.
///
/// `id_and_payload.id` is always the literal `"self"`. `id_and_payload.payload` is the
/// JSON-serialized `Frame::Account`. The per-row content fingerprint
/// (`payload_blake3`) that used to ride alongside is gone — translate
/// drives incremental skip via `dolt_diff_<table>` now; see
/// `super::super::translate::parse`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "account")]
pub struct AccountRow {
    pub id_and_payload: WirePayload,
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
/// - `payload` — JSONB of the `Frame::Recipient` message.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "recipients")]
pub struct RecipientRow {
    pub id_and_payload: WirePayload,
    pub identifier: Option<String>,
    pub display_name: Option<String>,
}

/// `chats` — one row per Signal chat (DM or group thread).
///
/// Columns:
/// - `id` — the in-backup `chat_id` (`uint64` upstream),
///   stringified. Primary key.
/// - `recipient_id` — promoted FK into [`RecipientRow`]; joins
///   `chats` to its peer / group without cracking the payload.
/// - `payload` — JSONB of the `Frame::Chat` message.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chats")]
pub struct ChatRow {
    pub id_and_payload: WirePayload,
    pub recipient_id: String,
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
/// - `chat_id` — promoted FK into [`ChatRow`].
/// - `author_id` — promoted FK into [`RecipientRow`].
/// - `date_sent` — upstream `chat_item.date_sent`, integer Unix-ms.
///   The closest thing this provider has to an event-shaped
///   timestamp; sourced into `GridRow.when_ts` by translate.
/// - `payload` — JSONB of the `Frame::ChatItem` message.
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
/// "all messages in a chat, ordered by time" query that translate
/// uses to materialize one document per chat.
pub const CHAT_ITEMS_BY_CHAT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS chat_items_by_chat ON chat_items(chat_id, date_sent)";

/// `chat_item_attachments` — N:M edge between one chat_item's
/// attachment slot and a `cas_objects` blob. Universal CAS-edge
/// shape (see [`frankweiler_etl::blob_cas::CasEdgeRow`]):
/// `id` (synth PK) + `chat_item_id` (owning FK, indexed) +
/// `ref_id` (= Signal `media_name`, indexed for the skip-check)
/// + `blake3` (CAS hash, NULL until decrypt+store succeed).
///
/// Replaces this provider's use of the shared `blob_refs` table;
/// see [issue #36](https://github.com/imbue-ai/mixed_up_files/issues/36).
///
/// **Signal-specific:** the PK recipe is `"{chat_item_id}#{slot}"`,
/// not the trait's default `"{chat_item_id}#{ref_id}"` — one
/// chat_item can attach the same `media_name` to multiple slots,
/// so the PK has to include the slot index. Signal calls
/// [`chat_item_attachment_id_recipe`] directly; the trait-derived
/// `pk_recipe` is not used for this table.
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

/// Recipe for the synthesized [`ChatItemRow`] primary key.
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
