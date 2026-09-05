//! Raw-store schema for the Beeper provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw as dr;
use datalib_etl_macros::CasEdgeRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &["rooms", "users", "events", "beeper_media_attachments"];

// rooms

/// `rooms` — one row per chat / channel / DM Beeper Texts knows
/// about.
///
/// PK choice: render-side UUIDv5 `beeper_room_uuid(source,
/// native_room_id)`. The native id (Matrix room id for index.db;
/// `chat.guid` for the future Mac chat.db reader) lives alongside as
/// its own column so cross-reference passes that arrive *after* the
/// row was written (e.g. the megabridge enrichment pass keyed off
/// `mxid`) can resolve back to the PK without recomputing the UUID.
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
/// network" filter that render / downstream tools use.
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
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
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

// users

/// `users` — one row per peer / participant Beeper Texts knows
/// about, across every chat in a given `source` store.
///
/// PK choice: render-side UUIDv5 `beeper_user_uuid(source,
/// native_user_id)`.
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
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
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

// events

/// `events` — one row per message / reaction / membership /
/// edit / hidden event Beeper Texts has cached.
///
/// PK choice: render-side UUIDv5 `beeper_event_uuid(source,
/// native_event_id)`. Both index.db and the megabridge file expose a
/// stable per-message Matrix event id (the `mxid` column), so the
/// UUIDv5 keyed off `(source, mxid)` is upstream-stable across
/// re-fetches.
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
/// events in a room, ordered by time" query render uses to
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
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
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

// beeper_media_attachments (CAS edge table)

/// `beeper_media_attachments` — N:M edge between one Beeper event
/// (`IMAGE` / `FILE` / avatar-bearing row) and a `cas_objects` blob.
/// Replaces this provider's use of the shared `blob_refs` table —
/// same universal four-column shape every other ported provider's
/// edge table uses (see
/// [`datalib_etl::blob_cas::CasEdgeRow`]).
///
/// PK choice: synthesized `"{event_uuid}#{ref_id}"` via the
/// universal `CasEdgeRow::pk_recipe`. One row per attachment slot
/// (a multi-image message becomes multiple rows, each with a
/// distinct `ref_id` like `"{event_uuid}:0"`, `"{event_uuid}:1"`,
/// …). Avatar attachments (one per user) reuse the shape with the
/// user's UUID as the owning id.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "beeper_media_attachments")]
pub struct BeeperMediaAttachmentRow {
    pub id: String,
    pub event_uuid: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

// UUIDv5 identity recipes

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

pub fn beeper_markdown_uuid(room_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:doc:{room_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}

// Composer

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
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
