//! Raw-store schema for the Slack provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture.md`](../../../../../docs/data_architecture.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! Slack-specific notes:
//!
//! - **Most entities key off the upstream Slack id directly**
//!   (`team_id`, `user_id`, `channel_id`). The wrinkle is `messages`:
//!   Slack history exposes `ts` which is unique only within a
//!   `(team, channel)` scope, so the PK is a UUIDv5 derived from
//!   `(team_id, channel_id, ts)` via [`slack_message_uuid`]. Threads
//!   are likewise keyed by [`slack_thread_uuid`]. Both recipes live
//!   in this file — per the plan §P0.4 raw-store PK recipes live
//!   next to the DDL constants that use them — and both extract and
//!   translate import from here, so the recipe can't drift between
//!   the writer and the reader.
//!
//! - **Event-shaped.** Slack messages carry an upstream `ts` field
//!   (unix-seconds-with-fraction, UTC); that's the event timestamp.
//!   `workspaces` / `users` / `channels` are not event-shaped (see
//!   [`docs/data_architecture.md`] §"Entities without a
//!   time-shape").
//!
//! - **`replies_pages` is a bookkeeping table**, not an entity: one
//!   row per `(channel_id, thread_ts)` for which we have a
//!   `conversations.replies` capture. Replies' actual bodies land
//!   in [`MESSAGES_DDL`] alongside top-level messages. The split
//!   exists so that "have we walked this thread?" is one cheap PK
//!   lookup and doesn't pollute the messages table with synthetic
//!   thread-marker rows.

use uuid::Uuid;

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity / bookkeeping tables, in the order they
/// should be iterated for full-table operations (truncate, full-DDL
/// composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs. Also drives [`full_ddl`] when it asks the shared
/// layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &[
    "workspaces",
    "users",
    "channels",
    "messages",
    "replies_pages",
];

/// `workspaces` — one row per Slack team (workspace).
///
/// Columns:
/// - `id` — upstream `team_id` from `auth.test`. Primary key.
/// - `team_name` — denormalized from the auth response for quick
///   display.
/// - `team_url` — the canonical workspace URL.
///   **FIXME** ([#9](https://github.com/imbue-ai/mixed_up_files/issues/9)):
///   downstream link rendering should use this when constructing
///   Slack outlinks; today some sites construct URLs without
///   consulting it.
/// - `self_user_id` — the user id `auth.test` resolved for us; lets
///   downstream code map "Me" without a second call.
/// - `payload` — raw `auth.test` response (JSONB-encoded on disk).
pub const WORKSPACES_DDL: &str = "CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY,
    team_name TEXT NULL,
    team_url TEXT NULL,
    self_user_id TEXT NULL,
    payload TEXT NULL
)";

/// `users` — one row per Slack user_id seen across any walked workspace.
///
/// Columns:
/// - `id` — upstream `user_id`. Primary key.
/// - `team_id` — denormalized owning workspace; lets cross-workspace
///   queries filter without cracking the payload.
/// - `name` — Slack handle.
/// - `real_name` — profile.real_name when present.
/// - `display_name` — profile.display_name when present.
/// - `payload` — raw `users.info` / `users.list` entry
///   (JSONB-encoded on disk).
pub const USERS_DDL: &str = "CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    team_id TEXT NULL,
    name TEXT NULL,
    real_name TEXT NULL,
    display_name TEXT NULL,
    payload TEXT NULL
)";

/// `channels` — one row per Slack chat surface: public channel,
/// private channel, DM (one-to-one), or **MPIM** (multi-party IM,
/// Slack's term for a group DM).
///
/// **Channels vs. conversations:** in Slack's wire vocabulary
/// "conversations" is the umbrella term covering all four surfaces
/// above, which is why this table's rows come from the
/// `conversations.info` / `conversations.list` endpoints and not
/// from anything called `channels.*`. We use the on-disk name
/// `channels` because it matches the surfaced user-facing concept;
/// the upstream API names are an implementation detail of where the
/// row came from.
///
/// Columns:
/// - `id` — upstream `channel_id`. Primary key.
/// - `name` — denormalized channel name.
/// - `is_member`, `is_archived` — denormalized flags from the
///   `conversations.info` payload that drive the listing filter and
///   the per-channel-sweep TTL semantics.
/// - `payload` — raw `conversations.info` / `conversations.list`
///   entry (JSONB-encoded on disk).
pub const CHANNELS_DDL: &str = "CREATE TABLE IF NOT EXISTS channels (
    id TEXT PRIMARY KEY,
    name TEXT NULL,
    is_member INTEGER NULL,
    is_archived INTEGER NULL,
    payload TEXT NULL
)";

/// `messages` — one row per Slack message (top-level or threaded
/// reply).
///
/// Columns:
/// - `id` — `slack_message_uuid(team_id, channel_id, ts)`. Primary
///   key. See [`slack_message_uuid`] for the recipe.
/// - `team_id`, `channel_id`, `ts` — the three components that
///   combine into `id`, kept as their own columns so queries that
///   filter or join by team/channel/ts (e.g. the
///   `messages_by_channel_ts` index, per-channel sweeps, sub-thread
///   walks) can use them directly. The v5 hash is one-way — without
///   keeping the inputs around, you couldn't recover them from
///   `id`.
/// - `thread_ts` — upstream `thread_ts` when this row is part of a
///   thread (root or reply); NULL for standalone messages.
/// - `thread_root_uuid` — for thread roots and standalone messages
///   the row's own `id`; for replies the
///   `slack_thread_uuid(team_id, channel_id, thread_ts)` of the
///   parent. Lets the translate side group a thread by a single
///   lookup. See [`slack_thread_uuid`] for the recipe.
/// - `is_thread_root` — 1 iff this row is the first message of a
///   thread.
/// - `user_id` — denormalized author for cheap "messages by X"
///   queries.
/// - `payload` — raw Slack message JSON, byte-for-byte (JSONB on
///   disk). The `ts` field inside the payload is the event
///   timestamp; the promoted `ts` column above carries the same
///   value for queryability.
pub const MESSAGES_DDL: &str = "CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    team_id TEXT NOT NULL,
    channel_id TEXT NOT NULL,
    ts TEXT NOT NULL,
    thread_ts TEXT NULL,
    thread_root_uuid TEXT NULL,
    is_thread_root INTEGER NULL,
    user_id TEXT NULL,
    payload TEXT NULL
)";

/// Index on `messages(channel_id, ts)` — supports the listing-style
/// "all messages in a channel, ordered by time" query without a
/// full table scan.
pub const MESSAGES_BY_CHANNEL_TS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS messages_by_channel_ts ON messages(channel_id, ts)";

/// Index on `messages(thread_root_uuid)` — supports the
/// "all messages in this thread" lookup that pulls a thread root
/// together with its replies in one query.
pub const MESSAGES_BY_THREAD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS messages_by_thread ON messages(thread_root_uuid)";

/// `replies_pages` — bookkeeping for `conversations.replies` walks.
///
/// One row per `(channel_id, thread_ts)` we have walked. Reply bodies
/// land in `messages`; this table tracks which threads we've already
/// walked and what the highwater reply ts was, so a re-run can decide
/// whether to ask Slack for more.
///
/// Columns:
/// - `id` — composite `"<channel_id>:<thread_ts>"` (see
///   [`replies_page_id_recipe`]). Primary key.
/// - `channel_id`, `thread_ts` — the two components of `id`, kept
///   alongside for join-without-parse queries.
/// - `latest_reply` — the most recent reply `ts` we've captured in
///   this thread; the next walk asks Slack for replies strictly
///   after this stamp.
pub const REPLIES_PAGES_DDL: &str = "CREATE TABLE IF NOT EXISTS replies_pages (
    id TEXT PRIMARY KEY,
    channel_id TEXT NOT NULL,
    thread_ts TEXT NOT NULL,
    latest_reply TEXT NULL
)";

/// Shared namespace for v5-derived Slack UUIDs.
///
/// Load-bearing because changing it would invalidate every uuid we
/// have ever produced for Slack — every `messages.id` already on
/// disk, every `GridRow.uuid` already projected, every backpointer
/// that names a slack message. So this byte sequence is effectively
/// immutable.
///
/// **FIXME**: a copy of this constant lives in
/// `src/ingest/providers/slack/parse.py` from the JSONL-era port.
/// Pretty sure that whole Python tree should be deleted; if any of
/// it is still needed to drive a fixture, port it to rust.
const SLACK_UUID_NS: Uuid = Uuid::from_bytes([
    0xa8, 0x9c, 0x7c, 0x4f, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

/// UUIDv5 recipe for a Slack message's PK.
///
/// Recipe: `uuidv5(SLACK_UUID_NS, "slack:msg:{team_id}:{channel_id}:{ts}")`.
///
/// Slack's wire format gives us `ts` which is only unique within a
/// `(team, channel)` scope, so we derive a stable UUIDv5 from the
/// three components. Deterministic across re-ingest (a
/// wipe-and-reingest of the same data yields the same uuid
/// byte-for-byte), and known before fetch (history pages supply
/// `ts`; `team_id` we cache from `auth.test`; the channel from the
/// listing).
pub fn slack_message_uuid(team_id: &str, channel_id: &str, ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:msg:{team_id}:{channel_id}:{ts}").as_bytes(),
    )
    .to_string()
}

/// UUIDv5 recipe for a Slack thread's stable identifier — populated
/// in `messages.thread_root_uuid` so all rows in a thread share one
/// value.
///
/// Recipe: `uuidv5(SLACK_UUID_NS, "slack:thread:{team_id}:{channel_id}:{thread_ts}")`.
///
/// `thread_ts` is the upstream `ts` of the thread root. For
/// standalone messages we fall back to the message's own
/// `slack_message_uuid` (handled by the caller in
/// `extract::db`), not by this recipe.
pub fn slack_thread_uuid(team_id: &str, channel_id: &str, thread_ts: &str) -> String {
    Uuid::new_v5(
        &SLACK_UUID_NS,
        format!("slack:thread:{team_id}:{channel_id}:{thread_ts}").as_bytes(),
    )
    .to_string()
}

/// Composite-key recipe for [`REPLIES_PAGES_DDL`]'s primary key.
/// Format: `"{channel_id}:{thread_ts}"`. Local to this file so the
/// PK format invariant lives next to the DDL that declares it.
pub fn replies_page_id_recipe(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
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
        WORKSPACES_DDL.to_string(),
        USERS_DDL.to_string(),
        CHANNELS_DDL.to_string(),
        MESSAGES_DDL.to_string(),
        MESSAGES_BY_CHANNEL_TS_INDEX_DDL.to_string(),
        MESSAGES_BY_THREAD_INDEX_DDL.to_string(),
        REPLIES_PAGES_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
