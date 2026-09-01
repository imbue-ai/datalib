//! Raw-store schema for the Slack provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/provider_migration_dolt_diff_and_cas_edge.md`] for the
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
//! [`datalib_etl::bulk::bulk_upsert_in_tx`] helper for writes.
//!
//! ## No listing pre-seed
//!
//! Rows only exist after a successful detail fetch (history, replies,
//! users.list, conversations.list, auth.test). See
//! [`docs/dev/data_architecture_ingestion.md`] §"No-preseed listing flow".
//!
//! ## Attachment bytes
//!
//! Attachment bytes live in the sibling per-source CAS file managed
//! by [`datalib_etl::blob_cas`]. The download path bulk-writes via
//! [`datalib_etl::blob_cas::BlobCas::put_many`] paired with a
//! bulk UPSERT into `slack_attachments`. The render path's per-thread
//! [`datalib_etl::blob_cas::BlobBundle`] joins `slack_attachments`
//! → `cas_objects` on `blake3`. Replaces this provider's use of the
//! shared `blob_refs` table.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;
use uuid::Uuid;

/// Names of the entity / bookkeeping tables, in the order they should
/// be iterated for full-table operations (truncate, full-DDL
/// composition, etc.). Used by `download::db::RawDb::reset` to wipe
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

/// Per-fetch volatile fields split out of the `users` content payload
/// into the `users_bookkeeping.volatile_payload` sidecar (see
/// [`datalib_etl::doltlite_raw::split_volatile`]). Slack stamps a
/// top-level `updated` epoch on every user object; it churns across
/// re-fetches without reflecting a state change, so it must not live in
/// the content payload that drives `dolt_diff_users`.
///
/// **Known gap, deliberately not fixed: `profile.status_*`.** A user's
/// Slack status — `status_text`, `status_emoji`, `status_expiration`,
/// `status_emoji_display_info` — is per-fetch state of the same kind as
/// `updated`, and it is still in the content payload. Nothing reads it
/// (`render::User::label` uses `real_name` / `name`), so when a
/// colleague sets or clears a status it produces a `dolt_diff_users`
/// change and a re-render that carry no information.
///
/// It also breaks the manual-e2e golden's `--reset-and-redownload`
/// stability check, which asserts that re-fetching unchanged upstream
/// objects lands identical bytes — observed 2026-08-31, when someone's
/// "In a meeting" status cleared partway through a bake.
///
/// Left alone because it is rare (it needs a status change inside the
/// ~90s a bake takes) and harmless when it happens: a spurious
/// re-render, not wrong data. If it starts costing bake reruns, the fix
/// is to add those four paths here — `split_volatile` already walks
/// nested paths, so `&["profile", "status_text"]` works as written.
pub const USER_VOLATILE_PATHS: &[dr::VolatilePath] = &[&["updated"]];

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
/// listing filter and per-channel-sweep TTL; `is_dm` / `dm_user_id`
/// do the same for the DM half. Full payload retained.
///
/// **A DM answers a different set of columns.** Checked against the
/// live API (2026-08-31): an `im` carries `user`, `is_archived` and
/// `is_user_deleted`, but no `name` and — the load-bearing gap — no
/// `is_member`, so the `members_only` predicate that selects channels
/// rejects every 1:1 DM. `is_dm` keeps the two populations apart in
/// one table: the `is_member` predicate runs only against `is_dm = 0`.
///
/// The new columns are added to already-existing stores by
/// [`datalib_etl::doltlite_raw::open`]'s schema reconcile.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "channels")]
pub struct ChannelRow {
    pub id_and_payload: WirePayload,
    // FIXME: Virtual column?
    pub name: Option<String>,
    //FIXME: define is_member (of what?)
    pub is_member: Option<i64>,
    pub is_archived: Option<i64>,
    /// 1 for a direct message surface — `is_im` (1:1) or `is_mpim`
    /// (group DM). 0 for a public or private channel.
    pub is_dm: Option<i64>,
    /// Who is in this DM, comma-joined, exactly as Slack listed them:
    /// an `im`'s single `user`, or an `mpim`'s `members` array (which
    /// *does* include the account itself). NULL for a channel.
    ///
    /// One column rather than an `im` field and an `mpim` field,
    /// because both surfaces answer the same two questions — is this a
    /// conversation with someone on the `dm_users` allowlist, and whose
    /// names title it — and a single participant list answers both for
    /// either shape. Self is subtracted at read time via
    /// [`dm_counterparts`] rather than at write time, so the column
    /// stays a faithful copy of the wire.
    pub dm_user_ids: Option<String>,
}

/// Split [`ChannelRow::dm_user_ids`] back into participant ids.
pub fn parse_dm_user_ids(joined: Option<&str>) -> Vec<String> {
    joined
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Join participant ids for [`ChannelRow::dm_user_ids`].
pub fn join_dm_user_ids(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(","))
    }
}

/// The people in a DM other than the account doing the mirroring.
///
/// An `mpim`'s `members` includes you; an `im`'s `user` does not. Both
/// are stored verbatim, so this is where the difference is reconciled —
/// once, for the allowlist and the display label alike. Falls back to
/// the full list when subtracting self would leave nothing, which is a
/// real case: a DM with yourself.
pub fn dm_counterparts(participants: &[String], self_user_id: Option<&str>) -> Vec<String> {
    let Some(me) = self_user_id else {
        return participants.to_vec();
    };
    let others: Vec<String> = participants.iter().filter(|u| *u != me).cloned().collect();
    if others.is_empty() {
        participants.to_vec()
    } else {
        others
    }
}

/// What to call a DM: `@` plus the people in it — `@Jean-Luc Picard`,
/// or `@William Riker, Data` for a group.
///
/// Shared by the downloader (progress lines, logs) and the renderer
/// (document titles, the grid's `conversation_name`) so the two can't
/// drift — a DM announced as one thing while syncing and titled
/// another once rendered reads as two different conversations.
///
/// `counterparts` comes from [`dm_counterparts`]; `labels` maps user id
/// → display name. The fallbacks matter: `name` is Slack's own
/// `mpdm-…` handle, and it is not split back into people because a
/// Slack handle may itself contain dashes. Reaching `channel_id` means
/// a store written before `dm_user_ids` existed, or a DM with someone
/// `users.list` didn't return.
pub fn dm_display_name(
    counterparts: &[String],
    name: Option<&str>,
    channel_id: &str,
    labels: &std::collections::BTreeMap<String, String>,
) -> String {
    if !counterparts.is_empty() {
        let names: Vec<&str> = counterparts
            .iter()
            .map(|u| labels.get(u).map(String::as_str).unwrap_or(u.as_str()))
            .collect();
        return format!("@{}", names.join(", "));
    }
    match name {
        Some(n) => format!("@{n}"),
        None => channel_id.to_string(),
    }
}

/// Per-fetch volatile fields split out of the `channels` content
/// payload into the `channels_bookkeeping.volatile_payload` sidecar
/// (see [`datalib_etl::doltlite_raw::split_volatile`]). Slack bumps
/// the top-level `updated` millis spuriously on every fetch, so leaving
/// it in the content payload would make `dolt_diff_channels` report a
/// change on every re-download — defeating incremental render and the
/// `--reset-and-redownload` "nothing changed" guarantee.
///
/// `num_members` belongs here for the same reason, and the manual-e2e
/// live golden is what proved it: a channel went 37 -> 38 members between
/// the cold run and the `--reset-and-redownload` run because somebody
/// joined while the test was running. It is a live membership counter, not
/// content — nobody re-renders a channel because its member count moved,
/// and leaving it in the content payload means `dolt_diff_channels` reports
/// a change every time anyone joins or leaves any mirrored channel.
pub const CHANNEL_VOLATILE_PATHS: &[dr::VolatilePath] = &[&["updated"], &["num_members"]];

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
/// on the render side.
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
/// use of the shared `blob_refs` table. Universal CAS-edge shape:
/// `id` (synth `"{message_uuid}#{file_id}"`), owning FK
/// (`message_uuid`, indexed so per-thread loads on the render side
/// stay cheap), upstream ref (`file_id`, also indexed for the
/// `blake3 IS NOT NULL` skip-check), `blake3` (null until the CAS
/// write lands). See [`datalib_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "slack_attachments")]
pub struct SlackAttachmentRow {
    pub id: String,
    pub message_uuid: String,
    pub file_id: String,
    pub blake3: Option<String>,
}

/// Shared namespace for v5-derived Slack UUIDs.
///
/// FIXME: We don't need this complexity around the namespace, it can use be a plain old string.  Let's make a backwards incompatible change to the schema and remove this.
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

/// UUIDv5 recipe for an individual reaction (one per reacting user) on
/// a message — its grid_row PK + markdown anchor.
///
/// Recipe: `uuidv5(SLACK_UUID_NS,
/// "slack:reaction:{team_id}:{channel_id}:{ts}:{name}:{user}")`.
pub fn slack_reaction_uuid(
    team_id: &str,
    channel_id: &str,
    ts: &str,
    name: &str,
    user: &str,
) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:reaction:{team_id}:{channel_id}:{ts}:{name}:{user}").as_bytes(),
    )
    .to_string()
}

/// Composite-key recipe for [`RepliesPagesRow`]'s primary key.
pub fn replies_page_id_recipe(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`].
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        WorkspaceRow::ddl(),
        UserRow::ddl(),
        ChannelRow::ddl(),
        MessageRow::ddl(),
        MESSAGES_BY_CHANNEL_TS_INDEX_DDL.to_string(),
        MESSAGES_BY_THREAD_INDEX_DDL.to_string(),
        REPLIES_PAGES_DDL.to_string(),
    ];
    out.extend(SlackAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
