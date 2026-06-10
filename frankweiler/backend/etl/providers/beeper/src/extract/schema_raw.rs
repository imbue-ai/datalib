//! Raw-store schema for the Beeper provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Beeper-specific notes:
//!
//! - **Local sqlite ingestion, not a live API.** Beeper reads the
//!   desktop app's on-disk SQLite tree under
//!   `~/Library/Application Support/BeeperTexts/` (primarily
//!   `index.db`, plus per-bridge `local-<bridge>/megabridge.db`
//!   files). There is no remote cursor, no listing endpoint, and no
//!   incremental "what changed since?" probe: every fetch walks the
//!   current sqlite snapshot end-to-end and relies on UPSERT dedup
//!   plus the shared `blob_refs` table to keep work bounded.
//!   Consequently there is no provider-local cursor table here; the
//!   shared bookkeeping sidecars are the only state we keep across
//!   runs.
//!
//! - **Multi-sourced.** Rows can come from `beeper_index` (the
//!   desktop app's unified cache, covering cloud bridges like Slack /
//!   Google Chat and local megabridges like Signal) or from a
//!   `beeper_megabridge_<network>` reader (cracks open the per-bridge
//!   `megabridge.db` to backfill upstream-canonical ids the desktop
//!   cache drops). The `source` column on every row records which
//!   on-disk store the row originated from; PKs are namespaced by
//!   `source` so two stores holding the "same" chat from different
//!   angles cannot collide. See [`beeper_room_uuid`-family recipes in
//!   `crate::translate`].
//!
//! - **Chat-human family with Slack / Signal.** Per
//!   `docs/data_architecture.md` §"Shared schemas across similar
//!   sources", Beeper is part of the chat-human cluster: `rooms` is
//!   the channel/thread/DM entity, `users` is the peer, `events` is
//!   the message-shaped child. `events.timestamp_ms` is the
//!   event-shaped value translate sources into `GridRow.when_ts`
//!   (Unix milliseconds, matching what Beeper / Matrix natively
//!   carry); sub-items lacking their own timestamp get a
//!   µs-bumped value derived from the parent per
//!   `docs/data_architecture.md`.
//!
//! - **`rooms` / `users` are not event-shaped.** They have no
//!   `when_ts` column; translate leaves `GridRow.when_ts` empty for
//!   them.
//!
//! - **PKs are translate-side UUIDv5.** The `id` columns are minted
//!   by `beeper_room_uuid` / `beeper_user_uuid` / `beeper_event_uuid`
//!   in `crate::translate`, keyed off `(source, native_id)`. The
//!   recipes live there because they're also consumed by the
//!   translate-side cross-reference logic; the writer in
//!   [`super::db`] calls into the same functions, so there is no
//!   duplication to lift here. When P0.4 lands and per-provider
//!   `uuid.rs` modules become standard, these will move there.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["rooms", "users", "events"];

/// `rooms` — one row per chat / channel / DM Beeper Texts knows
/// about.
///
/// Provenance: `index.db`'s `mx_room_metadata` / `threads` join,
/// surfaced by [`super::index_db`]. One row per Matrix room id (the
/// `native_room_id`) per `source` store.
///
/// PK choice: translate-side UUIDv5 `beeper_room_uuid(source,
/// native_room_id)`. The native id (Matrix room id for index.db;
/// `chat.guid` for the future Mac chat.db reader) lives alongside as
/// its own column so cross-reference passes that arrive *after* the
/// row was written (e.g. the megabridge enrichment pass keyed off
/// `mxid`) can resolve back to the PK without recomputing the UUID.
///
/// Native vs external ids: `native_room_id` is the room's identifier
/// inside Beeper's universe (the Matrix room id, e.g.
/// `!abc:beeper.local`). `external_room_id` and
/// `external_workspace_id` are the UPSTREAM system's canonical ids
/// (Signal conversation UUID, Slack channel id, Google Chat space
/// id, …) — what you'd use to talk to that service's own API.
/// Sourced from `thread.extra.bridge.*` when Beeper populates them.
///
/// Columns:
/// - `id` — UUIDv5 PK (see above). Primary key.
/// - `source` — which on-disk store this row came from
///   (`"beeper_index"`, `"beeper_megabridge_signal"`, …). Part of the
///   uniqueness key with `native_room_id`.
/// - `network` — canonical chat network (`"signal"`, `"googlechat"`,
///   `"slack"`, `"imessage"`, …) for downstream filtering & dispatch.
/// - `native_room_id` — Beeper-side room id (Matrix room id from
///   index.db, `chat.guid` from chat.db).
/// - `external_room_id` — upstream system's canonical room id.
/// - `external_workspace_id` — upstream workspace / team / account
///   id; `NULL` for bridges with a flat per-account namespace.
/// - `account_id` — Beeper-side bridge account id (`thread.accountID`
///   on index.db).
/// - `room_type` — Beeper-canonical taxonomy
///   (`"single"`, `"group"`, `"space"`, …).
/// - `title` — denormalized room title.
/// - `description` — denormalized room topic / description.
/// - `is_dm` — 1 if a 1:1 chat, 0 otherwise.
/// - `is_space` — 1 if a Matrix space (folder of rooms), 0 otherwise.
/// - `payload` — full upstream record, e.g. the `thread` JSON for
///   index.db (JSONB-encoded on disk).
pub const ROOMS_DDL: &str = "CREATE TABLE IF NOT EXISTS rooms (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    network TEXT NOT NULL,
    native_room_id TEXT NOT NULL,
    external_room_id TEXT NULL,
    external_workspace_id TEXT NULL,
    account_id TEXT NULL,
    room_type TEXT NULL,
    title TEXT NULL,
    description TEXT NULL,
    is_dm INTEGER NOT NULL DEFAULT 0,
    is_space INTEGER NOT NULL DEFAULT 0,
    payload TEXT NULL
)";

/// Unique index on `rooms(source, native_room_id)` — guarantees the
/// uniqueness of the `(source, native_id)` pair the UUIDv5 PK is
/// minted from, and supports the writer-side "have I already seen
/// this native room?" lookup and the megabridge enrichment pass that
/// joins by `native_room_id`.
pub const ROOMS_BY_SOURCE_NATIVE_INDEX_DDL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS rooms_by_source_native ON rooms(source, native_room_id)";

/// Index on `rooms.network` — supports the "all rooms for this
/// network" filter that translate / downstream tools use.
pub const ROOMS_BY_NETWORK_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS rooms_by_network ON rooms(network)";

/// `users` — one row per peer / participant Beeper Texts knows
/// about, across every chat in a given `source` store.
///
/// Provenance: `index.db`'s `mx_room_members` / users join, surfaced
/// by [`super::index_db`]. The same Matrix user id may legitimately
/// appear across rooms; we dedupe on `(source, native_user_id)`.
///
/// PK choice: translate-side UUIDv5 `beeper_user_uuid(source,
/// native_user_id)`.
///
/// Columns:
/// - `id` — UUIDv5 PK. Primary key.
/// - `source` — which on-disk store this row came from. Part of the
///   uniqueness key with `native_user_id`.
/// - `network` — canonical chat network for this user, when known.
///   `NULL` when the membership row alone doesn't carry it (e.g.
///   imported contacts).
/// - `native_user_id` — Beeper-side user id (Matrix user id from
///   index.db).
/// - `display_name` — denormalized chat-display name (per-room or
///   per-account).
/// - `full_name` — denormalized profile-level name when distinct from
///   `display_name`.
/// - `remote_id` — upstream system's canonical user id (Signal ACI,
///   Slack user id, …) when Beeper propagates it.
/// - `avatar_blob_id` — blake3 ref into `blob_refs` for a cached
///   profile image, when present.
/// - `payload` — full upstream record (JSONB-encoded on disk).
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    network TEXT NULL,
    native_user_id TEXT NOT NULL,
    display_name TEXT NULL,
    full_name TEXT NULL,
    remote_id TEXT NULL,
    avatar_blob_id TEXT NULL,
    payload TEXT NULL
)";

/// Unique index on `users(source, native_user_id)` — guarantees the
/// uniqueness of the `(source, native_id)` pair the UUIDv5 PK is
/// minted from, and supports the writer-side dedup probe.
pub const USERS_BY_SOURCE_NATIVE_INDEX_DDL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS users_by_source_native ON users(source, native_user_id)";

/// `events` — one row per message / reaction / membership /
/// edit / hidden event Beeper Texts has cached.
///
/// Provenance: `index.db`'s `mx_room_messages` + `mx_reactions` (with
/// per-row `source = "beeper_index"`), plus optional backfill from
/// `local-<bridge>/megabridge.db` (with `source =
/// "beeper_megabridge_<network>"`) that populates
/// `external_event_id` keyed off `mxid`.
///
/// PK choice: translate-side UUIDv5 `beeper_event_uuid(source,
/// native_event_id)`. Both index.db and the megabridge file expose a
/// stable per-message Matrix event id (the `mxid` column), so the
/// UUIDv5 keyed off `(source, mxid)` is upstream-stable across
/// re-fetches.
///
/// `external_event_id` is reserved for the upstream system's
/// canonical message id (Signal message UUID, Slack `ts`, etc.). It
/// is **not** populated from `index.db` — Beeper doesn't propagate
/// the underlying network's per-message ids into the desktop cache.
/// The column exists here so future bridges-DB or cloud-API readers
/// can backfill it without a schema bump.
///
/// Columns:
/// - `id` — UUIDv5 PK. Primary key.
/// - `source` — which on-disk store this row came from.
/// - `network` — canonical chat network for this event.
/// - `room_uuid` — FK into [`ROOMS_DDL`]; equals
///   `beeper_room_uuid(source, native_room_id)`.
/// - `sender_uuid` — FK into [`USERS_DDL`]; equals
///   `beeper_user_uuid(source, native_sender_user_id)`. `NULL` for
///   system events with no clear sender.
/// - `native_event_id` — Beeper-side event id (Matrix `mxid`).
/// - `external_event_id` — upstream system's canonical message id;
///   `NULL` until the megabridge pass backfills it.
/// - `event_type` — Beeper-canonical taxonomy (`"TEXT"`, `"IMAGE"`,
///   `"FILE"`, `"REACTION"`, `"MEMBERSHIP"`, `"HIDDEN"`, …) — same
///   labels the desktop app uses in `mx_room_messages.type`.
/// - `timestamp_ms` — upstream send time in Unix milliseconds.
///   Sourced into `GridRow.when_ts` by translate (after conversion
///   to ISO-8601).
/// - `text_content` — promoted plain-text body, when present.
/// - `reply_to_native_event_id` — Matrix `mxid` this event replies
///   to; threading anchor.
/// - `edit_of_native_event_id` — Matrix `mxid` this event edits.
/// - `reaction_emoji` — promoted reaction emoji for
///   `event_type = "REACTION"` rows.
/// - `reaction_target_native_event_id` — Matrix `mxid` the reaction
///   targets.
/// - `payload` — full upstream record (JSONB-encoded on disk).
pub const EVENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    network TEXT NOT NULL,
    room_uuid TEXT NOT NULL,
    sender_uuid TEXT NULL,
    native_event_id TEXT NOT NULL,
    external_event_id TEXT NULL,
    event_type TEXT NOT NULL,
    timestamp_ms INTEGER NOT NULL,
    text_content TEXT NULL,
    reply_to_native_event_id TEXT NULL,
    edit_of_native_event_id TEXT NULL,
    reaction_emoji TEXT NULL,
    reaction_target_native_event_id TEXT NULL,
    payload TEXT NULL
)";

/// Index on `events(room_uuid, timestamp_ms)` — supports the "all
/// events in a room, ordered by time" query translate uses to
/// materialize one document per room.
pub const EVENTS_BY_ROOM_TS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS events_by_room_ts ON events(room_uuid, timestamp_ms)";

/// Index on `events(source, native_event_id)` — supports the
/// megabridge enrichment pass that joins by `(source, mxid)` to
/// backfill `external_event_id` without scanning.
pub const EVENTS_BY_SOURCE_NATIVE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS events_by_source_native ON events(source, native_event_id)";

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
        ROOMS_DDL.to_string(),
        ROOMS_BY_SOURCE_NATIVE_INDEX_DDL.to_string(),
        ROOMS_BY_NETWORK_INDEX_DDL.to_string(),
        USERS_DDL.to_string(),
        USERS_BY_SOURCE_NATIVE_INDEX_DDL.to_string(),
        EVENTS_DDL.to_string(),
        EVENTS_BY_ROOM_TS_INDEX_DDL.to_string(),
        EVENTS_BY_SOURCE_NATIVE_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
