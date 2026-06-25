//! Raw-store schema for the Beeper provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/archived/data_architecture_plan.md`](/docs/dev/archived/data_architecture_plan.md)
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
//!   plus the per-provider CAS edge table to keep work bounded.
//!   Consequently there is no provider-local cursor table here.
//!
//! - **The source `.db` files ARE the backup.** Per
//!   `docs/dev/data_architecture_ingestion.md` §"Schema first", a
//!   provider whose upstream is already on disk doesn't need to
//!   slavishly preserve every byte — re-running extract is cheap
//!   and the source file remains untouched. So Beeper drops the
//!   per-row `payload` JSONB column it used to keep: every field
//!   render actually wants is already promoted to a typed column,
//!   and a future read that wants something more obscure can
//!   re-extract from `index.db` directly.
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
//!   `crate::render_and_index_md`].
//!
//! - **Chat-human family with Slack / Signal.** Per
//!   `docs/dev/data_architecture_ingestion.md` §"Shared schemas across similar
//!   sources", Beeper is part of the chat-human cluster: `rooms` is
//!   the channel/thread/DM entity, `users` is the peer, `events` is
//!   the message-shaped child. `events.timestamp_ms` is the
//!   event-shaped value translate sources into `GridRow.when_ts`
//!   (Unix milliseconds, matching what Beeper / Matrix natively
//!   carry); sub-items lacking their own timestamp get a
//!   µs-bumped value derived from the parent per
//!   `docs/dev/data_architecture_ingestion.md`.
//!
//! - **`rooms` / `users` are not event-shaped.** They have no
//!   `when_ts` column; translate leaves `GridRow.when_ts` empty for
//!   them.
//!
//! - **PKs are translate-side UUIDv5.** The `id` columns are minted
//!   by `beeper_room_uuid` / `beeper_user_uuid` / `beeper_event_uuid`
//!   in `crate::render_and_index_md`, keyed off `(source, native_id)`. The
//!   recipes live there because they're also consumed by the
//!   translate-side cross-reference logic; the writer here calls
//!   into the same functions.

use frankweiler_etl::blob_cas::CasEdgeRow as _;
use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw as dr;
use frankweiler_etl_macros::CasEdgeRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["rooms", "users", "events", "beeper_media_attachments"];

// ─────────────────────────────────────────────────────────────────────
// rooms
// ─────────────────────────────────────────────────────────────────────

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
/// - `native_room_id` — Beeper-side room id.
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
    is_space INTEGER NOT NULL DEFAULT 0
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

/// Row matching [`ROOMS_DDL`]. Hand-rolled `BulkUpsertable` (no
/// `payload` column — see the file-level docstring).
#[derive(Debug, Clone, Default)]
pub struct RoomRow {
    pub id: String,
    pub source: String,
    pub network: String,
    pub native_room_id: String,
    pub external_room_id: Option<String>,
    pub external_workspace_id: Option<String>,
    pub account_id: Option<String>,
    pub room_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub is_dm: bool,
    pub is_space: bool,
}

impl BulkUpsertable for RoomRow {
    const TABLE: &'static str = "rooms";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "source",
        "network",
        "native_room_id",
        "external_room_id",
        "external_workspace_id",
        "account_id",
        "room_type",
        "title",
        "description",
        "is_dm",
        "is_space",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.source)
            .bind(&self.network)
            .bind(&self.native_room_id)
            .bind(self.external_room_id.as_deref())
            .bind(self.external_workspace_id.as_deref())
            .bind(self.account_id.as_deref())
            .bind(self.room_type.as_deref())
            .bind(self.title.as_deref())
            .bind(self.description.as_deref())
            .bind(self.is_dm as i64)
            .bind(self.is_space as i64)
    }
}

// ─────────────────────────────────────────────────────────────────────
// users
// ─────────────────────────────────────────────────────────────────────

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
/// - `avatar_blob_id` — `ref_id` into
///   [`beeper_media_attachments`](BeeperMediaAttachmentRow) for a
///   cached profile image, when present.
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    network TEXT NULL,
    native_user_id TEXT NOT NULL,
    display_name TEXT NULL,
    full_name TEXT NULL,
    remote_id TEXT NULL,
    avatar_blob_id TEXT NULL
)";

/// Unique index on `users(source, native_user_id)` — guarantees the
/// uniqueness of the `(source, native_id)` pair the UUIDv5 PK is
/// minted from, and supports the writer-side dedup probe.
pub const USERS_BY_SOURCE_NATIVE_INDEX_DDL: &str =
    "CREATE UNIQUE INDEX IF NOT EXISTS users_by_source_native ON users(source, native_user_id)";

/// Row matching [`USERS_DDL`]. Hand-rolled `BulkUpsertable` (no
/// `payload` column).
#[derive(Debug, Clone, Default)]
pub struct UserRow {
    pub id: String,
    pub source: String,
    pub network: Option<String>,
    pub native_user_id: String,
    pub display_name: Option<String>,
    pub full_name: Option<String>,
    pub remote_id: Option<String>,
    pub avatar_blob_id: Option<String>,
}

impl BulkUpsertable for UserRow {
    const TABLE: &'static str = "users";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "source",
        "network",
        "native_user_id",
        "display_name",
        "full_name",
        "remote_id",
        "avatar_blob_id",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.source)
            .bind(self.network.as_deref())
            .bind(&self.native_user_id)
            .bind(self.display_name.as_deref())
            .bind(self.full_name.as_deref())
            .bind(self.remote_id.as_deref())
            .bind(self.avatar_blob_id.as_deref())
    }
}

// ─────────────────────────────────────────────────────────────────────
// events
// ─────────────────────────────────────────────────────────────────────

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
///
/// Attachment payloads (`IMAGE` / `FILE` events) are NOT stored
/// here. Each attachment lands as one row in
/// [`beeper_media_attachments`](BeeperMediaAttachmentRow), keyed by
/// `(event_uuid, attachment_id)`, with the bytes in the sibling CAS.
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
    reaction_target_native_event_id TEXT NULL
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

/// Row matching [`EVENTS_DDL`]. Hand-rolled `BulkUpsertable` (no
/// `payload` column).
///
/// **Schema honesty caveat:** `reaction_emoji` and
/// `reaction_target_native_event_id` are only meaningful when
/// `event_type = 'REACTION'`; `text_content` and `reply_to_*` /
/// `edit_of_*` make sense for `'TEXT'` / `'IMAGE'` / `'FILE'` rows
/// but not for `'REACTION'` / `'MEMBERSHIP'` / `'HIDDEN'`. The DDL
/// doesn't enforce these per-`event_type` subsets — it can't,
/// without a per-type table split — and a reader of the schema has
/// to consult this docstring (and the
/// [`crate::render_and_index_md::render`] taxonomy switch) to know which
/// columns apply when.
///
/// We pay this cost deliberately to keep the
/// "everything-on-one-timeline" rendering shape:
/// `SELECT … FROM events WHERE room_uuid = ? ORDER BY timestamp_ms`
/// in one indexed scan is the load-bearing operation for both
/// period-bucketing and reaction-to-message attachment. A
/// per-`event_type` split (`beeper_message` / `beeper_reaction` /
/// `beeper_membership` / …) would force a `UNION` or a join-heavy
/// rewrite of the bucket walker in
/// [`crate::render_and_index_md::parse`] and lose the chronological
/// ordering convenience the unified table buys us.
///
/// If a future need pushes us toward stricter typing, the natural
/// re-split is along `event_type` — the docstring above already
/// lists the canonical taxonomy and most columns map cleanly to
/// one or two subtables.
#[derive(Debug, Clone, Default)]
pub struct EventRow {
    pub id: String,
    pub source: String,
    pub network: String,
    pub room_uuid: String,
    pub sender_uuid: Option<String>,
    pub native_event_id: String,
    pub external_event_id: Option<String>,
    pub event_type: String,
    pub timestamp_ms: i64,
    pub text_content: Option<String>,
    pub reply_to_native_event_id: Option<String>,
    pub edit_of_native_event_id: Option<String>,
    pub reaction_emoji: Option<String>,
    pub reaction_target_native_event_id: Option<String>,
}

impl BulkUpsertable for EventRow {
    const TABLE: &'static str = "events";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "source",
        "network",
        "room_uuid",
        "sender_uuid",
        "native_event_id",
        "external_event_id",
        "event_type",
        "timestamp_ms",
        "text_content",
        "reply_to_native_event_id",
        "edit_of_native_event_id",
        "reaction_emoji",
        "reaction_target_native_event_id",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.source)
            .bind(&self.network)
            .bind(&self.room_uuid)
            .bind(self.sender_uuid.as_deref())
            .bind(&self.native_event_id)
            .bind(self.external_event_id.as_deref())
            .bind(&self.event_type)
            .bind(self.timestamp_ms)
            .bind(self.text_content.as_deref())
            .bind(self.reply_to_native_event_id.as_deref())
            .bind(self.edit_of_native_event_id.as_deref())
            .bind(self.reaction_emoji.as_deref())
            .bind(self.reaction_target_native_event_id.as_deref())
    }
}

// ─────────────────────────────────────────────────────────────────────
// beeper_media_attachments (CAS edge table)
// ─────────────────────────────────────────────────────────────────────

/// `beeper_media_attachments` — N:M edge between one Beeper event
/// (`IMAGE` / `FILE` / avatar-bearing row) and a `cas_objects` blob.
/// Replaces this provider's use of the shared `blob_refs` table —
/// same universal four-column shape every other ported provider's
/// edge table uses (see
/// [`frankweiler_etl::blob_cas::CasEdgeRow`]).
///
/// PK choice: synthesized `"{event_uuid}#{ref_id}"` via the
/// universal `CasEdgeRow::pk_recipe`. One row per attachment slot
/// (a multi-image message becomes multiple rows, each with a
/// distinct `ref_id` like `"{event_uuid}:0"`, `"{event_uuid}:1"`,
/// …). Avatar attachments (one per user) reuse the shape with the
/// user's UUID as the owning id.
///
/// Columns:
/// - `id` — synthesized PK.
/// - `event_uuid` — owning event FK (or user UUID for avatars).
///   Indexed so the per-bucket
///   [`frankweiler_etl::blob_cas::BlobBundle::load`] projection on
///   the render side stays cheap.
/// - `ref_id` — Beeper-side attachment id (the desktop app's
///   `attachment.id`, typically a `mxc://…` URL). Indexed against
///   `blake3` so the "have we got these bytes yet?" skip check is
///   one row read.
/// - `blake3` — CAS hash of the bytes, NULL until the file copy
///   completes.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "beeper_media_attachments")]
pub struct BeeperMediaAttachmentRow {
    pub id: String,
    pub event_uuid: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────
// UUIDv5 identity recipes
// ─────────────────────────────────────────────────────────────────────

/// v5 namespace for every UUID this provider mints. Distinct from
/// other providers so we can never accidentally collide a Beeper
/// row with a Slack/Notion/etc. row that happened to derive its
/// id from the same string.
pub const BEEPER_UUID_NS: Uuid = Uuid::from_bytes([
    0xbe, 0xe9, 0xe7, 0x00, 0x4f, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

/// `source` is the on-disk store the row came from (e.g.
/// `"beeper_index"`, eventually `"macos_imessage"`). Including it
/// in the v5 hash means two extractors that happen to mint
/// identical native ids never collide unless that's actually
/// meaningful.
pub fn beeper_room_uuid(source: &str, native_room_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:room:{source}:{native_room_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_user_uuid(source: &str, native_user_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:user:{source}:{native_user_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_event_uuid(source: &str, native_event_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:event:{source}:{native_event_id}").as_bytes(),
    )
    .to_string()
}

/// Per-period document UUID. Stable for the lifetime of the
/// `(room, period)` pair regardless of how many times we re-render
/// — so the load step can foreign-key against it consistently.
pub fn beeper_markdown_uuid(room_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:doc:{room_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Composer
// ─────────────────────────────────────────────────────────────────────

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, the CAS-edge DDLs, and the
/// paired `<table>_bookkeeping` DDL produced by the shared layer.
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
    out.extend(BeeperMediaAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
