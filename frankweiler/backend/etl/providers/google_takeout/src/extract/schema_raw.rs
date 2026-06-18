//! Raw-store schema for the Google Takeout provider.
//!
//! Declarations-only. All row structs derive
//! [`frankweiler_etl_macros::WirePayloadRow`] (entity tables) or
//! [`frankweiler_etl_macros::CasEdgeRow`] (per-provider CAS edge
//! tables), so the DDL + `BulkUpsertable` plumbing comes from the
//! macros and this file is just the schema description.
//!
//! ## Tables
//!
//! Entity (wire-payload) tables — each gets a `_bookkeeping` sidecar
//! courtesy of [`frankweiler_etl::doltlite_raw::bookkeeping_ddl_for`]:
//!
//!   - `maps_reviews`, `maps_saved_places`, `maps_photos`
//!   - `youtube_watch_history`, `youtube_subscriptions`
//!   - `chat_groups`, `chat_users`, `chat_messages`
//!   - `gemini_activity`
//!
//! CAS-edge tables — each maps `(owning_id, ref_id) → blake3`:
//!
//!   - `chat_attachments`     — owning `message_id`,  ref `export_name`
//!   - `gemini_attachments`   — owning `activity_id`, ref `filename`
//!
//! `maps_photos` is structurally an attachment but conceptually a
//! first-class entity (the photo *is* the row), so it lives as a
//! wire-payload entity table with a `blake3` column rather than a
//! separate edge table. The bytes still ride through the shared
//! `cas_objects` CAS the same way.
//!
//! ## Identity
//!
//! See `docs/dev/google_takeout_ingestion.md` § "Identity / Ship-of-Theseus"
//! for the per-table PK recipes. Where Google gives us a stable id
//! (Chat `message_id`, YouTube `Channel Id`, photo file-stem) we use
//! it verbatim; where it doesn't we synthesize a uuidv5 from the
//! most stable available fields, namespaced under
//! [`google_takeout_ns`].

use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::{CasEdgeRow, WirePayloadRow};
use uuid::Uuid;

/// Entity tables — each gets a paired `<table>_bookkeeping` sidecar.
/// CAS edge tables (`chat_attachments`, `gemini_attachments`) live in
/// [`EDGE_TABLES`] so reset can wipe their bookkeeping the same way.
pub const DATA_TABLES: &[&str] = &[
    "maps_reviews",
    "maps_saved_places",
    "maps_photos",
    "youtube_watch_history",
    "youtube_subscriptions",
    "chat_groups",
    "chat_users",
    "chat_messages",
    "gemini_activity",
    // Google Voice feed (own `schema_raw`); see `google_voice::schema_raw`.
    "voice_messages",
    "voice_bills",
    "voice_greetings",
];

/// Per-provider CAS edge tables. Wiped by reset alongside
/// [`DATA_TABLES`].
pub const EDGE_TABLES: &[&str] = &[
    "chat_attachments",
    "gemini_attachments",
    "voice_attachments",
];

/// Per-provider uuidv5 namespace constant. Recipes are kebab-ish
/// strings (`"maps_review:{ftid}:{date}"`, `"youtube:watch:{id}:{ts}"`,
/// …) hashed under this namespace; the resulting Uuid is the row's
/// `id`.
pub fn google_takeout_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"google-takeout.frankweiler")
}

/// Build a uuidv5-derived id string for a recipe under the
/// provider's namespace. The recipe is the only spec-stable input;
/// callers should document the shape next to the call site.
pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&google_takeout_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

// ── Maps ────────────────────────────────────────────────────────────

/// `maps_reviews` — one row per review the user wrote.
///
/// PK recipe: `uuidv5(NS, "maps_review:{ftid}:{date}")` where `ftid`
/// is the hex id after `!1s` in the place's `google_maps_url`. A
/// user can review the same place twice; `(ftid, date)` is the
/// smallest natural key.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "maps_reviews")]
pub struct MapsReviewRow {
    pub id_and_payload: WirePayload,
    pub when_ts: Option<String>,
}

/// `maps_saved_places` — one row per "saved place" (starred, wantgo,
/// list entry, etc.). PK recipe:
/// `uuidv5(NS, "maps_saved:{ftid_or_cid}:{date}")`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "maps_saved_places")]
pub struct MapsSavedPlaceRow {
    pub id_and_payload: WirePayload,
    pub when_ts: Option<String>,
}

/// `maps_photos` — one row per photo the user uploaded to a place.
///
/// PK is the photo file-stem (e.g. `2026-06-04-af8bb6e0`). Bytes
/// land in `cas_objects` keyed by `blake3`; `blake3` here is the
/// content hash of the JPEG bytes.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "maps_photos")]
pub struct MapsPhotoRow {
    pub id_and_payload: WirePayload,
    pub when_ts: Option<String>,
    pub blake3: Option<String>,
}

// ── YouTube ─────────────────────────────────────────────────────────

/// `youtube_watch_history` — one row per video watched.
///
/// PK recipe: `uuidv5(NS, "youtube:watch:{video_id}:{iso_ts}")`. The
/// payload carries the parsed cell fields (`video_url`, `video_id`,
/// `video_title`, `channel_*`, raw `when_str`). The original cell's
/// HTML is *not* retained per row — it's the same MDL boilerplate
/// for every entry; the full file lives on disk at `input_path`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "youtube_watch_history")]
pub struct YoutubeWatchRow {
    pub id_and_payload: WirePayload,
    pub when_ts: Option<String>,
    pub video_id: Option<String>,
    pub channel_id: Option<String>,
}

/// `youtube_subscriptions` — one row per channel the user subscribes
/// to. PK is the upstream `Channel Id` (`UC…`). Not event-shaped; no
/// `when_ts`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "youtube_subscriptions")]
pub struct YoutubeSubscriptionRow {
    pub id_and_payload: WirePayload,
    pub channel_title: Option<String>,
}

// ── Google Chat ─────────────────────────────────────────────────────

/// `chat_groups` — one row per Google Chat space/DM. PK is the
/// takeout directory name (`"DM 2ZwEI8AAAAE"`, `"Space Foo"`); the
/// payload is the verbatim `group_info.json`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chat_groups")]
pub struct ChatGroupRow {
    pub id_and_payload: WirePayload,
}

/// `chat_users` — one row per Google Chat user surfaced under
/// `Users/User <id>/user_info.json`. PK is the directory name. The
/// payload is the verbatim `user_info.json` — translate uses
/// `payload.user.email` to identify the self in DM display-name
/// derivation.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chat_users")]
pub struct ChatUserRow {
    pub id_and_payload: WirePayload,
}

/// `chat_messages` — one row per chat message.
///
/// PK is the upstream `message_id` verbatim (it's globally unique:
/// `{group}/{topic}/{msg}`). `group_id` references the owning
/// `chat_groups.id`. `sender_email` is promoted off the payload so
/// per-sender queries don't have to crack the JSON.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "chat_messages")]
pub struct ChatMessageRow {
    pub id_and_payload: WirePayload,
    pub group_id: String,
    pub sender_email: Option<String>,
    pub when_ts: Option<String>,
}

/// `chat_attachments` — per-provider CAS edge for attachments
/// referenced by chat messages. Owning entity: `message_id`. Ref:
/// `export_name` (the filename Takeout used to land the bytes
/// alongside `messages.json`).
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "chat_attachments")]
pub struct ChatAttachmentRow {
    pub id: String,
    pub message_id: String,
    pub export_name: String,
    pub blake3: Option<String>,
}

// ── Gemini Apps ─────────────────────────────────────────────────────

/// `gemini_activity` — one row per Gemini Apps conversation cell.
///
/// PK recipe: `uuidv5(NS, "gemini:" + blake3_hex(prompt + "\0" +
/// when_str))`. The MDL HTML has no machine id per entry; the cell
/// timestamp + prompt text is the smallest natural key. blake3 is
/// stable across re-exports and matches the rest of the codebase's
/// content-hash discipline.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "gemini_activity")]
pub struct GeminiActivityRow {
    pub id_and_payload: WirePayload,
    pub when_ts: Option<String>,
}

/// `gemini_attachments` — per-provider CAS edge for files Gemini
/// surfaced inline in a cell. Owning entity: `activity_id`. Ref:
/// `filename` (the sibling file the cell's anchor pointed at).
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "gemini_attachments")]
pub struct GeminiAttachmentRow {
    pub id: String,
    pub activity_id: String,
    pub filename: String,
    pub blake3: Option<String>,
}

// ── DDL composition ────────────────────────────────────────────────

/// Full DDL list passed to [`frankweiler_etl::doltlite_raw::open`].
/// Composes every table + its bookkeeping sidecar, plus the shared
/// `ingested_files` cursor table.
pub fn full_ddl() -> Vec<String> {
    use frankweiler_etl::blob_cas::CasEdgeRow as _;
    let mut out: Vec<String> = vec![
        MapsReviewRow::ddl(),
        MapsSavedPlaceRow::ddl(),
        MapsPhotoRow::ddl(),
        YoutubeWatchRow::ddl(),
        YoutubeSubscriptionRow::ddl(),
        ChatGroupRow::ddl(),
        ChatUserRow::ddl(),
        ChatMessageRow::ddl(),
        GeminiActivityRow::ddl(),
        // Shared file-cursor table; we own one or more
        // `google_takeout/<feed>` scopes inside it.
        frankweiler_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    out.extend(ChatAttachmentRow::all_ddl());
    out.extend(GeminiAttachmentRow::all_ddl());
    // Google Voice feed's own tables (entity + CAS edge DDL); the
    // bookkeeping loop below covers its `_bookkeeping` sidecars.
    out.extend(super::google_voice::schema_raw::voice_table_ddl());
    for table in DATA_TABLES.iter().chain(EDGE_TABLES.iter()) {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankweiler_etl::bulk::BulkUpsertable;

    #[test]
    fn namespace_is_stable() {
        // Stable bytes-to-bytes: same recipe always hashes to the
        // same id across runs and platforms.
        let a = ns_id("maps_review:abc:2026-06-04");
        let b = ns_id("maps_review:abc:2026-06-04");
        assert_eq!(a, b);
        assert_eq!(a.len(), 36); // hyphenated uuid
    }

    #[test]
    fn distinct_recipes_distinct_ids() {
        assert_ne!(
            ns_id("maps_review:abc:2026-06-04"),
            ns_id("maps_review:abc:2026-06-05"),
        );
    }

    #[test]
    fn full_ddl_compiles_every_table() {
        let stmts = full_ddl();
        let blob = stmts.join("\n");
        for t in DATA_TABLES.iter().chain(EDGE_TABLES.iter()) {
            assert!(blob.contains(t), "missing DDL for {t}");
            assert!(
                blob.contains(&format!("{t}_bookkeeping")),
                "missing bookkeeping DDL for {t}",
            );
        }
        assert!(blob.contains("ingested_files"));
    }

    #[test]
    fn cas_edge_columns_match_design() {
        assert_eq!(ChatAttachmentRow::TABLE, "chat_attachments");
        assert_eq!(GeminiAttachmentRow::TABLE, "gemini_attachments");
        // TYPED_COLUMNS for a CasEdgeRow are (owning, ref, blake3).
        assert_eq!(
            ChatAttachmentRow::TYPED_COLUMNS,
            &["message_id", "export_name", "blake3"]
        );
        assert_eq!(
            GeminiAttachmentRow::TYPED_COLUMNS,
            &["activity_id", "filename", "blake3"]
        );
    }
}
