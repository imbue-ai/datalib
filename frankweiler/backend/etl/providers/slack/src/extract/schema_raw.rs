//! Raw-store schema for the Slack provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/provider_migration_dolt_diff_and_cas_edge.md`] for the
//! conventions every `schema_raw.rs` follows.
//!
//! Slack-specific notes:
//!
//! - **Most entities key off the upstream Slack id directly**
//!   (`team_id`, `user_id`, `channel_id`). The wrinkle is `messages`:
//!   Slack history exposes `ts` which is unique only within a
//!   `(team, channel)` scope, so the PK is a UUIDv5 derived from
//!   `(team_id, channel_id, ts)` via [`slack_message_uuid`]. Threads
//!   are likewise keyed by [`slack_thread_uuid`]. Both recipes live
//!   in this file so the writer and the reader can't drift.
//!
//! - **`replies_pages` is a bookkeeping table**, not an entity: one
//!   row per `(channel_id, thread_ts)` for which we have a
//!   `conversations.replies` capture. Bodies land in [`MessageRow`]
//!   alongside top-level messages. Doesn't fit `WirePayloadRow` (no
//!   wire payload), so it's hand-rolled as `BulkUpsertable`.
//!
//! ## Row structs and the bulk-upsert path
//!
//! `WorkspaceRow`, `UserRow`, `ChannelRow`, `MessageRow` derive
//! [`WirePayloadRow`] (field `id_and_payload: WirePayload`) — the
//! macro emits both the DDL and the [`BulkUpsertable`] impl. The two
//! non-payload tables (`RepliesPagesRow`, `SlackAttachmentRow`) hand-
//! roll `BulkUpsertable`. All six tables go through the generic
//! [`frankweiler_etl::bulk::bulk_upsert_in_tx`] helper for writes.
//!
//! ## No listing pre-seed
//!
//! Rows only exist after a successful detail fetch (history, replies,
//! users.list, conversations.list, auth.test). See
//! [`docs/data_architecture_ingestion.md`] §"No-preseed listing flow".
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS file managed
//! by [`frankweiler_etl::blob_cas`]. The extract path bulk-writes via
//! [`frankweiler_etl::blob_cas::BlobCas::put_many`] paired with a
//! bulk UPSERT into `slack_attachments`. The render path's per-thread
//! [`frankweiler_etl::blob_cas::BlobBundle`] joins `slack_attachments`
//! → `cas_objects` on `blake3`. Replaces this provider's use of the
//! shared `blob_refs` table.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

/// Names of the entity / bookkeeping tables, in the order they should
/// be iterated for full-table operations (truncate, full-DDL
/// composition, etc.). Used by `extract::db::RawDb::reset` to wipe
/// per-row state without touching blobs.
pub const DATA_TABLES: &[&str] = &[
    "workspaces",
    "users",
    "channels",
    "messages",
    "replies_pages",
    "slack_attachments",
];

/// `workspaces` — one row per Slack team (workspace).
///
/// Columns: `team_name`, `team_url`, `self_user_id` denormalized from
/// the `auth.test` response; full payload retained.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "workspaces")]
pub struct WorkspaceRow {
    pub id_and_payload: WirePayload,
    pub team_name: Option<String>,
    pub team_url: Option<String>,
    pub self_user_id: Option<String>,
}

/// `users` — one row per Slack user_id seen across any walked workspace.
///
/// Columns: `team_id`, `name`, `real_name`, `display_name`
/// denormalized for cheap label queries; full payload retained.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "users")]
pub struct UserRow {
    pub id_and_payload: WirePayload,
    pub team_id: Option<String>,
    // FIXME: Can these be VIRTUAL columns based on the JSONB from the payload?
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
}

/// `channels` — one row per Slack chat surface: public channel,
/// private channel, DM, or MPIM.
///
/// **Channels vs. conversations:** in Slack's wire vocabulary
/// "conversations" is the umbrella term covering all four surfaces;
/// we use `channels` because it matches the user-facing concept. The
/// upstream API names (`conversations.info` / `conversations.list`)
/// are an implementation detail of where the payload came from.
///
/// Columns: `name`, `is_member`, `is_archived` drive the
/// listing filter and per-channel-sweep TTL; full payload retained.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "channels")]
pub struct ChannelRow {
    pub id_and_payload: WirePayload,
    // FIXME: Virtual column?
    pub name: Option<String>,
    //FIXME: define is_member (of what?)
    pub is_member: Option<i64>,
    pub is_archived: Option<i64>,
}

/// `messages` — one row per Slack message (top-level or threaded
/// reply).
///
/// Columns:
/// - `id` — `slack_message_uuid(team_id, channel_id, ts)`. The v5
///   hash is one-way, so the three components stay as their own
///   columns for cross-table queries.
/// - `team_id`, `channel_id`, `ts` — the three v5 inputs.
/// - `thread_ts` — upstream `thread_ts` when this row is part of a
///   thread (root or reply); NULL for standalone messages.
/// - `thread_root_uuid` — `slack_thread_uuid(team_id, channel_id,
///   effective_thread_ts)`. For standalone messages, the effective
///   thread_ts is the message's own ts, so every row has a non-NULL
///   value — the `messages_by_thread` index covers everything.
/// - `is_thread_root` — 1 iff this row is the first message of a
///   thread.
/// - `user_id` — denormalized author for cheap "messages by X" queries.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "messages")]
pub struct MessageRow {
    pub id_and_payload: WirePayload,
    // FIXME: Can some of these be VIRTUAL columns?
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub thread_root_uuid: String,
    pub is_thread_root: i64,
    pub user_id: Option<String>,
}

/// Index on `messages(channel_id, ts)` — supports the listing-style
/// "all messages in a channel, ordered by time" query without a
/// full table scan.
pub const MESSAGES_BY_CHANNEL_TS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS messages_by_channel_ts ON messages(channel_id, ts)";

/// Index on `messages(thread_root_uuid)` — supports per-thread loads
/// on the translate side.
pub const MESSAGES_BY_THREAD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS messages_by_thread ON messages(thread_root_uuid)";

/// `replies_pages` — bookkeeping for `conversations.replies` walks.
///
/// One row per `(channel_id, thread_ts)` we have walked. Reply bodies
/// land in `messages`; this table tracks the highwater reply ts so a
/// re-run can decide whether to ask Slack for more.
///
/// // FIXME: Seems like we could have a utility to generate the SQL and BulkUpsertable impl from the struct below (we may have to annotated it a bit more?)
/// Hand-rolled `BulkUpsertable` (no wire payload).
pub const REPLIES_PAGES_DDL: &str = "CREATE TABLE IF NOT EXISTS replies_pages (
    id           TEXT PRIMARY KEY,
    channel_id   TEXT NOT NULL,
    thread_ts    TEXT NOT NULL,
    latest_reply TEXT NULL
)";

#[derive(Debug, Clone)]
pub struct RepliesPagesRow {
    pub id: String,
    pub channel_id: String,
    pub thread_ts: String,
    pub latest_reply: Option<String>,
}

impl BulkUpsertable for RepliesPagesRow {
    const TABLE: &'static str = "replies_pages";
    const TYPED_COLUMNS: &'static [&'static str] = &["channel_id", "thread_ts", "latest_reply"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.channel_id)
            .bind(&self.thread_ts)
            .bind(self.latest_reply.as_deref())
    }
}

/// `slack_attachments` — N:M edge between one Slack message's
/// attachment slot and a `cas_objects` blob. Replaces this provider's
/// use of the shared `blob_refs` table.
///
/// Columns:
/// - `id` — synthesized PK `"{message_uuid}#{file_id}"`.
/// - `message_uuid` — FK into [`MessageRow`]. Explicit so the dolt_diff
///   union can project the natural bucket key (`thread_root_uuid`) by
///   joining through `messages` without parsing the synthesized PK.
/// - `file_id` — upstream Slack `file_id`. Skip-check key:
///   `(file_id, blake3 IS NOT NULL)` means we already have the bytes.
/// - `blake3` — CAS content hash, NULL until the CAS write succeeds.
pub const SLACK_ATTACHMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS slack_attachments (
    id           TEXT PRIMARY KEY,
    message_uuid TEXT NOT NULL,
    file_id      TEXT NOT NULL,
    blake3       TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

pub const SLACK_ATTACHMENTS_BY_MSG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS slack_attachments_by_msg \
     ON slack_attachments(message_uuid)";

pub const SLACK_ATTACHMENTS_BY_FILE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS slack_attachments_by_file \
     ON slack_attachments(file_id, blake3)";

#[derive(Debug, Clone)]
pub struct SlackAttachmentRow {
    pub id: String,
    pub message_uuid: String,
    pub file_id: String,
    pub blake3: Option<String>,
}

impl BulkUpsertable for SlackAttachmentRow {
    const TABLE: &'static str = "slack_attachments";
    const TYPED_COLUMNS: &'static [&'static str] = &["message_uuid", "file_id", "blake3"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.message_uuid)
            .bind(&self.file_id)
            .bind(self.blake3.as_deref())
    }
}

/// Shared namespace for v5-derived Slack UUIDs.
///
/// Load-bearing because changing it would invalidate every uuid we
/// have ever produced for Slack. So this byte sequence is effectively
/// immutable.
const SLACK_UUID_NS: Uuid = Uuid::from_bytes([
    0xa8, 0x9c, 0x7c, 0x4f, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

/// UUIDv5 recipe for a Slack message's PK.
///
/// Recipe: `uuidv5(SLACK_UUID_NS, "slack:msg:{team_id}:{channel_id}:{ts}")`.
pub fn slack_message_uuid(team_id: &str, channel_id: &str, ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:msg:{team_id}:{channel_id}:{ts}").as_bytes(),
    )
    .to_string()
}

/// UUIDv5 recipe for a Slack thread's stable identifier.
///
/// Recipe: `uuidv5(SLACK_UUID_NS, "slack:thread:{team_id}:{channel_id}:{thread_ts}")`.
pub fn slack_thread_uuid(team_id: &str, channel_id: &str, thread_ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:thread:{team_id}:{channel_id}:{thread_ts}").as_bytes(),
    )
    .to_string()
}

/// Composite-key recipe for [`RepliesPagesRow`]'s primary key.
pub fn replies_page_id_recipe(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
}

/// Recipe for the synthesized [`SLACK_ATTACHMENTS_DDL`] PK.
pub fn attachment_id_recipe(message_uuid: &str, file_id: &str) -> String {
    format!("{message_uuid}#{file_id}")
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        WorkspaceRow::ddl(),
        UserRow::ddl(),
        ChannelRow::ddl(),
        MessageRow::ddl(),
        MESSAGES_BY_CHANNEL_TS_INDEX_DDL.to_string(),
        MESSAGES_BY_THREAD_INDEX_DDL.to_string(),
        REPLIES_PAGES_DDL.to_string(),
        SLACK_ATTACHMENTS_DDL.to_string(),
        SLACK_ATTACHMENTS_BY_MSG_INDEX_DDL.to_string(),
        SLACK_ATTACHMENTS_BY_FILE_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
