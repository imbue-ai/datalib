//! Raw-store schema for the Slack provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Names of the entity / bookkeeping tables, in the order they should
/// be iterated for full-table operations (truncate, full-DDL
/// composition, etc.). Used by `ingest::db::RawDb::reset` to wipe
/// per-row state without touching blobs.
pub const DATA_TABLES: &[&str] = &[
    "workspaces",
    "users",
    "channels",
    "messages",
    "replies_pages",
    "slack_attachments",
    "channel_read_states",
    "bookmarks",
    "saved_items",
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
/// The `profile.status_*` and `profile.huddle_*` fields say what the
/// person is doing at this instant, and a mirror that keeps only the
/// latest value is keeping a random sample of it. A calendar
/// integration flipping someone to "In a meeting" would otherwise read
/// as a modified user: re-rendered, re-indexed, and churning the grid.
/// Nothing in the tree reads them.
pub const USER_VOLATILE_PATHS: &[dr::VolatilePath] = &[
    &["updated"],
    &["profile", "status_text"],
    &["profile", "status_text_canonical"],
    &["profile", "status_emoji"],
    &["profile", "status_emoji_display_info"],
    &["profile", "status_expiration"],
    &["profile", "huddle_state"],
    &["profile", "huddle_state_expiration_ts"],
];

/// `channels` — one row per Slack chat surface: public channel,
/// private channel, DM, or MPIM.
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
    pub dm_user_ids: Option<String>,
}

pub fn parse_dm_user_ids(joined: Option<&str>) -> Vec<String> {
    joined
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

pub fn join_dm_user_ids(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(","))
    }
}

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
/// reset-then-resync "nothing changed" guarantee.
pub const CHANNEL_VOLATILE_PATHS: &[dr::VolatilePath] = &[&["updated"], &["num_members"]];

/// `messages` — one row per Slack message (top-level or threaded
/// reply).
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

/// Per-fetch account state split out of the `messages` content payload
/// into `messages_bookkeeping.volatile_payload`. Slack puts both on the
/// root of a thread the account follows, and only in the copy
/// `conversations.replies` returns: `last_read` is how far into the
/// thread the account has read, `subscribed` whether it follows it.
/// Neither is a change to the message. The `conversations.history` copy
/// of the same root carries neither, and an upsert that splits nothing
/// leaves the sidecar as it was, so the two copies no longer overwrite
/// each other.
pub const MESSAGE_VOLATILE_PATHS: &[dr::VolatilePath] = &[&["last_read"], &["subscribed"]];

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
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
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

/// `channel_read_states` — how far the account has read in each
/// mirrored conversation, one row per conversation, from `client.counts`.
///
/// Every field but `id` is volatile ([`READ_STATE_VOLATILE_PATHS`]):
/// reading a channel is not a change to it, so the content payload is
/// just `{"id": …}` and the state lives in
/// `channel_read_states_bookkeeping.volatile_payload`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "channel_read_states")]
pub struct ChannelReadStateRow {
    pub id_and_payload: WirePayload,
}

/// The fields of a `client.counts` entry, as the live API returned them
/// on 2026-09-24. `last_read` and `latest` are message `ts`es; `updated`
/// and `history_invalid` are `ts`-shaped stamps Slack keeps for its own
/// cache.
pub const READ_STATE_VOLATILE_PATHS: &[dr::VolatilePath] = &[
    &["last_read"],
    &["latest"],
    &["updated"],
    &["history_invalid"],
    &["mention_count"],
    &["has_unreads"],
];

/// `bookmarks` — the links and pinned messages in a conversation's
/// header bar, from `bookmarks.list`. A bookmark inside a folder names
/// the folder in `parent_id`; the folder itself is not listed (its label
/// is a tab in the channel's `properties.tabs`).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "bookmarks")]
pub struct BookmarkRow {
    pub id_and_payload: WirePayload,
    pub channel_id: String,
}

pub const BOOKMARKS_BY_CHANNEL_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS bookmarks_by_channel ON bookmarks(channel_id)";

/// `saved_items` — the account's "Saved for later" list, from
/// `saved.list`: in progress, completed and archived alike. A saved
/// message is a pointer (`item_id` is its conversation, `ts` the
/// message), not a copy of it.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "saved_items")]
pub struct SavedItemRow {
    pub id_and_payload: WirePayload,
    pub item_type: String,
    pub item_id: String,
    pub ts: Option<String>,
}

pub const SAVED_ITEMS_BY_ITEM_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS saved_items_by_item ON saved_items(item_id)";

pub fn saved_item_key(item_type: &str, item_id: &str, ts: Option<&str>) -> String {
    format!("{item_type}#{item_id}#{}", ts.unwrap_or(""))
}

/// The raw store's keys: the upstream's own, joined with `#`. A
/// message is `{team}#{channel}#{ts}`, a thread the same over its root's
/// `ts`, so a channel's messages sort by time and a sync's new ones
/// land at its tail. Not an entity id — those carry the configured
/// source and are minted by the render (`docs/dev/entity_ids.md`).
pub fn slack_message_key(team_id: &str, channel_id: &str, ts: &str) -> String {
    format!("{team_id}#{channel_id}#{ts}")
}

pub fn slack_thread_key(team_id: &str, channel_id: &str, thread_ts: &str) -> String {
    format!("{team_id}#{channel_id}#{thread_ts}")
}

/// The three parts of a [`slack_thread_key`], for a render that has
/// only the key in hand — the driver names stale buckets by it.
pub fn split_thread_key(key: &str) -> Option<(&str, &str, &str)> {
    let mut it = key.splitn(3, '#');
    Some((it.next()?, it.next()?, it.next()?))
}

/// Composite-key recipe for [`RepliesPagesRow`]'s primary key.
pub fn replies_page_id_recipe(channel_id: &str, thread_ts: &str) -> String {
    format!("{channel_id}:{thread_ts}")
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        WorkspaceRow::ddl(),
        UserRow::ddl(),
        ChannelRow::ddl(),
        MessageRow::ddl(),
        MESSAGES_BY_CHANNEL_TS_INDEX_DDL.to_string(),
        MESSAGES_BY_THREAD_INDEX_DDL.to_string(),
        REPLIES_PAGES_DDL.to_string(),
        ChannelReadStateRow::ddl(),
        BookmarkRow::ddl(),
        BOOKMARKS_BY_CHANNEL_INDEX_DDL.to_string(),
        SavedItemRow::ddl(),
        SAVED_ITEMS_BY_ITEM_INDEX_DDL.to_string(),
    ];
    out.extend(SlackAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn user_with_status(text: &str, emoji: &str, expiration: i64) -> serde_json::Value {
        json!({
            "id": "U0239BBA55M",
            "name": "picard",
            "updated": 1788879000,
            "profile": {
                "real_name": "Jean-Luc Picard",
                "title": "Captain",
                "status_text": text,
                "status_text_canonical": text,
                "status_emoji": emoji,
                "status_emoji_display_info": if emoji.is_empty() { json!([]) } else { json!([{"emoji_name": "tea"}]) },
                "status_expiration": expiration,
            },
        })
    }

    /// A person whose Slack status moved — and nothing else — must land
    /// the same content payload, or `dolt_diff_users` calls them
    /// modified, the render re-runs, and the grid churns.
    ///
    /// The manual-e2e bake caught this for real on 2026-09-08: a
    /// calendar integration flipped someone to "In a meeting" between
    /// two runs four minutes apart, and the reset-then-resync
    /// stability check failed on four `profile.status_*` paths.
    #[test]
    fn a_status_change_does_not_move_the_content_payload() {
        let quiet = user_with_status("", "", 0);
        let busy = user_with_status("In a meeting • Google Calendar", ":tea:", 1788879000);

        let (quiet_base, quiet_volatile) = dr::split_volatile(&quiet, USER_VOLATILE_PATHS);
        let (busy_base, busy_volatile) = dr::split_volatile(&busy, USER_VOLATILE_PATHS);

        assert_eq!(
            quiet_base, busy_base,
            "the status fields reached the content payload"
        );
        assert_ne!(
            quiet_volatile, busy_volatile,
            "the status went nowhere — it belongs in the sidecar, not dropped"
        );
        assert_eq!(
            busy_base["profile"]["title"], "Captain",
            "the split took more than the status"
        );
    }
}
