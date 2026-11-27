//! The doltlite raw store, and the one way each kind of record is
//! written into it.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::{bulk_upsert_first_seen_in_tx, bulk_upsert_in_tx, BulkUpsertable};
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::key_range::prefix_end;
use datalib_etl::sweep::Sweeps;
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::schema_raw::*;
use super::snowflake;
use datalib_sql::{sql, sql_id, Sql};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

type Tx<'a> = Transaction<'a, Sqlite>;

pub fn id_of(record: &Value) -> Option<&str> {
    record.get("id")?.as_str().filter(|s| !s.is_empty())
}

/// How a write stamps the sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stamp {
    /// Read for the first time: stamp the fetch.
    Fetched,
    /// A record the answer carries whether or not it changed: stamp the
    /// first time only, so an unchanged one commits nothing.
    FirstSeen,
}

struct Split {
    id: String,
    payload: String,
    volatile: Option<Value>,
}

fn split(id: String, record: &Value, by: fn(&Value) -> (Value, Option<Value>)) -> Result<Split> {
    let (base, volatile) = by(record);
    let payload = serde_json::to_string(&base).with_context(|| format!("serialize {id}"))?;
    Ok(Split {
        id,
        payload,
        volatile,
    })
}

/// The content of `rows`, then what each gave up to its sidecar, merged
/// onto what is there: two endpoints answer one record with different
/// fields.
async fn write<T: BulkUpsertable>(
    tx: &mut Tx<'_>,
    rows: &[T],
    volatile: &[(String, Value)],
    stamp: Stamp,
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    match stamp {
        Stamp::Fetched => bulk_upsert_in_tx(tx, rows, now).await?,
        Stamp::FirstSeen => bulk_upsert_first_seen_in_tx(tx, rows, now).await?,
    }
    let pairs: Vec<(&str, &Value)> = volatile.iter().map(|(id, v)| (id.as_str(), v)).collect();
    dr::merge_volatile_payloads_in_tx(tx, T::TABLE, &pairs).await
}

macro_rules! simple_write {
    ($name:ident, $row:ident, $split:ident) => {
        pub async fn $name(
            tx: &mut Tx<'_>,
            records: &[(&str, &Value)],
            now: &IsoOffsetTimestamp,
        ) -> Result<()> {
            let mut rows = Vec::with_capacity(records.len());
            let mut volatile = Vec::new();
            for (id, record) in records {
                let s = split(id.to_string(), record, $split)?;
                volatile.extend(s.volatile.map(|v| (s.id.clone(), v)));
                rows.push($row {
                    id_and_payload: WirePayload {
                        id: s.id,
                        payload: s.payload,
                    },
                });
            }
            write(tx, &rows, &volatile, Stamp::FirstSeen, now).await
        }
    };
}

simple_write!(write_account, AccountRow, split_account);
simple_write!(write_users, UserRow, split_user);
simple_write!(write_guilds, GuildRow, split_guild);
simple_write!(write_member, MemberRow, split_member);

/// A server's roles, the ones it no longer has deleted: the listing is
/// not paged.
pub async fn write_roles(
    tx: &mut Tx<'_>,
    guild_id: &str,
    roles: &[Value],
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let mut rows = Vec::with_capacity(roles.len());
    for role in roles {
        let Some(id) = id_of(role) else { continue };
        let s = split(id.to_string(), role, |r| (r.clone(), None))?;
        rows.push(RoleRow {
            id_and_payload: WirePayload {
                id: s.id,
                payload: s.payload,
            },
            guild_id: guild_id.to_string(),
        });
    }
    let listed: Vec<&str> = rows.iter().map(|r| r.id_and_payload.id.as_str()).collect();
    let listed = serde_json::to_string(&listed)?;
    for sql in [
        sql!(
            "discord.write_roles.delete_roles_bookkeeping",
            "DELETE FROM roles_bookkeeping WHERE id IN (SELECT id FROM roles \
         WHERE guild_id = ?1 AND id NOT IN (SELECT value FROM json_each(?2)))"
        ),
        sql!(
            "discord.write_roles.delete_roles",
            "DELETE FROM roles WHERE guild_id = ?1 AND id NOT IN (SELECT value FROM json_each(?2))"
        ),
    ] {
        sql.query()
            .bind(guild_id)
            .bind(&listed)
            .execute(&mut **tx)
            .await
            .context("drop the roles a server no longer has")?;
    }
    write(tx, &rows, &[], Stamp::FirstSeen, now).await
}

pub async fn write_channels(
    tx: &mut Tx<'_>,
    channels: &[&Value],
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let text = |c: &Value, key: &str| c.get(key).and_then(Value::as_str).map(String::from);
    let mut rows = Vec::with_capacity(channels.len());
    let mut volatile = Vec::new();
    for channel in channels {
        let Some(id) = id_of(channel) else { continue };
        let s = split(id.to_string(), channel, split_channel)?;
        volatile.extend(s.volatile.map(|v| (s.id.clone(), v)));
        rows.push(ChannelRow {
            id_and_payload: WirePayload {
                id: s.id,
                payload: s.payload,
            },
            guild_id: text(channel, "guild_id"),
            parent_id: text(channel, "parent_id"),
            kind: channel.get("type").and_then(Value::as_i64),
        });
    }
    write(tx, &rows, &volatile, Stamp::FirstSeen, now).await
}

/// One conversation's messages, with an edge for each file they carry.
pub async fn write_messages(
    tx: &mut Tx<'_>,
    channel_id: &str,
    messages: &[&Value],
    stamp: Stamp,
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let mut rows = Vec::with_capacity(messages.len());
    let mut volatile = Vec::new();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    for message in messages {
        let Some(id) = id_of(message).and_then(snowflake::parse) else {
            continue;
        };
        let s = split(message_key(channel_id, id), message, split_message)?;
        for file in files_of(message) {
            let Some(file_id) = id_of(file) else { continue };
            edges.push((
                DiscordAttachmentRow::pk_recipe(&s.id, file_id),
                s.id.clone(),
                file_id.to_string(),
            ));
        }
        volatile.extend(s.volatile.map(|v| (s.id.clone(), v)));
        rows.push(MessageRow {
            id_and_payload: WirePayload {
                id: s.id,
                payload: s.payload,
            },
            channel_id: channel_id.to_string(),
        });
    }
    write(tx, &rows, &volatile, stamp, now).await?;
    for (id, message_key, attachment_id) in &edges {
        sql!(
            "discord.insert_attachment_edge",
            "INSERT INTO discord_attachments (id, message_key, attachment_id) VALUES (?, ?, ?) \
             ON CONFLICT(id) DO NOTHING"
        )
        .query()
        .bind(id)
        .bind(message_key)
        .bind(attachment_id)
        .execute(&mut **tx)
        .await
        .context("write a file's edge")?;
    }
    Ok(())
}

/// The files a message carries, a forwarded message's among them.
pub fn files_of(message: &Value) -> Vec<&Value> {
    let own = message.get("attachments").and_then(Value::as_array);
    let forwarded = message
        .get("message_snapshots")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("message")?.get("attachments")?.as_array());
    own.into_iter().chain(forwarded).flatten().collect()
}

/// Deletes the stored messages of `channel_id` from `lo` up to, not
/// including, `below` that are not among `seen`; their edges go with
/// them. How many went.
pub async fn prune_messages(
    tx: &mut Tx<'_>,
    channel_id: &str,
    lo: u64,
    below: Option<u64>,
    seen: &[u64],
) -> Result<usize> {
    let lo = message_key(channel_id, lo);
    let end = match below {
        Some(b) => message_key(channel_id, b),
        None => prefix_end(&format!("{channel_id}#")).context("a channel's key range")?,
    };
    let seen: Vec<String> = seen.iter().map(|id| message_key(channel_id, *id)).collect();
    let gone: Vec<String> = sql!(
        "discord.list_messages_gone_upstream",
        "SELECT id FROM messages WHERE id >= ? AND id < ? \
         AND id NOT IN (SELECT value FROM json_each(?))"
    )
    .query_scalar()
    .bind(&lo)
    .bind(&end)
    .bind(serde_json::to_string(&seen)?)
    .fetch_all(&mut **tx)
    .await
    .context("find the messages deleted upstream")?;
    if gone.is_empty() {
        return Ok(0);
    }
    let keys = serde_json::to_string(&gone)?;
    for sql in [
        sql!(
            "discord.prune_messages.delete_discord_attachments_bookkeeping",
            "DELETE FROM discord_attachments_bookkeeping WHERE id IN (SELECT id FROM \
         discord_attachments WHERE message_key IN (SELECT value FROM json_each(?)))"
        ),
        sql!(
            "discord.prune_messages.delete_discord_attachments",
            "DELETE FROM discord_attachments WHERE message_key IN (SELECT value FROM json_each(?))"
        ),
        sql!(
            "discord.prune_messages.delete_messages_bookkeeping",
            "DELETE FROM messages_bookkeeping WHERE id IN (SELECT value FROM json_each(?))"
        ),
        sql!(
            "discord.prune_messages.delete_messages",
            "DELETE FROM messages WHERE id IN (SELECT value FROM json_each(?))"
        ),
    ] {
        sql.query()
            .bind(&keys)
            .execute(&mut **tx)
            .await
            .context("delete what was deleted upstream")?;
    }
    for key in &gone {
        datalib_etl::prune::forget_record_problems_in_tx(tx, MESSAGES, key).await?;
    }
    Ok(gone.len())
}

/// Whether any message of `channel_id` made at `floor` or later is held.
pub async fn holds_messages_since(tx: &mut Tx<'_>, channel_id: &str, floor: u64) -> Result<bool> {
    let end = prefix_end(&format!("{channel_id}#")).context("a channel's key range")?;
    let n: Option<i64> = sql!(
        "discord.find_message_in_refresh_window",
        "SELECT 1 FROM messages WHERE id >= ? AND id < ? LIMIT 1"
    )
    .query_scalar()
    .bind(message_key(channel_id, floor))
    .bind(end)
    .fetch_optional(&mut **tx)
    .await
    .context("look for messages in the refresh window")?;
    Ok(n.is_some())
}

/// Every stored thread of `parents`, with its newest message as last
/// listed.
pub async fn stored_threads(
    tx: &mut Tx<'_>,
    parents: &[&str],
) -> Result<Vec<(String, Option<u64>)>> {
    let rows: Vec<(String, Option<String>)> = sql!(
        "discord.list_threads_with_last_message",
        "SELECT c.id, json_extract(b.volatile_payload, '$.last_message_id') \
         FROM channels c LEFT JOIN channels_bookkeeping b ON b.id = c.id \
         WHERE c.parent_id IN (SELECT value FROM json_each(?)) AND c.kind IN (10, 11, 12) \
         ORDER BY c.id"
    )
    .query_as()
    .bind(serde_json::to_string(parents)?)
    .fetch_all(&mut **tx)
    .await
    .context("list the stored threads")?;
    Ok(rows
        .into_iter()
        .map(|(id, tip)| (id, tip.as_deref().and_then(snowflake::parse)))
        .collect())
}

/// A stored payload of one of the store's tables.
pub async fn stored_payload(
    tx: &mut Tx<'_>,
    table: &'static str,
    id: &str,
) -> Result<Option<Value>> {
    // Audited: `table` is one of this module's own names.
    let sql = Sql::dynamic(
        sql_id!("discord.read_payload_by_id"),
        format!("SELECT json(payload) FROM {table} WHERE id = ?"),
    );
    let payload: Option<Option<String>> = sql
        .query_scalar()
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .with_context(|| format!("read {table}={id}"))?;
    Ok(payload
        .flatten()
        .and_then(|p| serde_json::from_str(&p).ok()))
}

pub async fn stored_roles(tx: &mut Tx<'_>, guild_id: &str) -> Result<Vec<Value>> {
    let payloads: Vec<Option<String>> = sql!(
        "discord.list_roles_of_server",
        "SELECT json(payload) FROM roles WHERE guild_id = ? ORDER BY id"
    )
    .query_scalar()
    .bind(guild_id)
    .fetch_all(&mut **tx)
    .await
    .context("read a server's roles")?;
    Ok(payloads
        .into_iter()
        .flatten()
        .filter_map(|p| serde_json::from_str(&p).ok())
        .collect())
}

/// A file owed: which message carries it, and what that message says of
/// it.
#[derive(Debug, Clone)]
pub struct OwedFile {
    pub channel_id: String,
    pub message_id: u64,
    /// As the message has it, its signed link laid back in.
    pub file: Value,
}

#[derive(sqlx::FromRow)]
struct OwedRow {
    id: String,
    message_key: String,
    attachment_id: String,
    payload: Option<String>,
    volatile: Option<String>,
}

/// Deletes the `account` row, and its sidecar, of any account but `id`:
/// the table holds one row, the account signed in now.
pub async fn delete_other_accounts(tx: &mut Tx<'_>, id: &str) -> Result<()> {
    for sql in [
        sql!(
            "discord.forget_other_accounts.delete_account_bookkeeping",
            "DELETE FROM account_bookkeeping WHERE id <> ?"
        ),
        sql!(
            "discord.forget_other_accounts.delete_account",
            "DELETE FROM account WHERE id <> ?"
        ),
    ] {
        sql.query()
            .bind(id)
            .execute(&mut **tx)
            .await
            .context("forget the account signed in before")?;
    }
    Ok(())
}

/// The edges with no bytes: those of `message_keys`, or every one.
pub async fn owed_files(
    tx: &mut Tx<'_>,
    message_keys: Option<&[String]>,
) -> Result<BTreeMap<String, OwedFile>> {
    const OWED: &str = "SELECT a.id, a.message_key, a.attachment_id, \
                json(m.payload) AS payload, json(b.volatile_payload) AS volatile \
         FROM discord_attachments a INDEXED BY discord_attachments_owed \
         LEFT JOIN messages m ON m.id = a.message_key \
         LEFT JOIN messages_bookkeeping b ON b.id = a.message_key \
         WHERE a.blake3 IS NULL";
    // Audited: `OWED` is the constant above, and what it is asked for is bound.
    let rows: Vec<OwedRow> = match message_keys {
        None => {
            Sql::dynamic(sql_id!("discord.list_owed_files"), OWED)
                .query_as()
                .fetch_all(&mut **tx)
                .await
        }
        Some(keys) => {
            let sql = Sql::dynamic(
                sql_id!("discord.list_owed_files_of_messages"),
                format!("{OWED} AND a.message_key IN (SELECT value FROM json_each(?1))"),
            );
            sql.query_as()
                .bind(serde_json::to_string(keys)?)
                .fetch_all(&mut **tx)
                .await
        }
    }
    .context("list the files with no bytes")?;
    let mut out = BTreeMap::new();
    for row in rows {
        let Some((channel, message)) = split_message_key(&row.message_key) else {
            continue;
        };
        let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok());
        let message_value = match (parse(row.payload), parse(row.volatile)) {
            (Some(p), Some(v)) => dr::overlay(&p, &v),
            (Some(p), None) => p,
            _ => Value::Null,
        };
        let file = files_of(&message_value)
            .into_iter()
            .find(|f| id_of(f) == Some(row.attachment_id.as_str()))
            .cloned()
            .unwrap_or(Value::Null);
        out.insert(
            row.id,
            OwedFile {
                channel_id: channel.to_string(),
                message_id: snowflake::parse(&message).unwrap_or_default(),
                file,
            },
        );
    }
    Ok(out)
}

/// `attachment id → blake3` of every file held, under any message.
pub async fn held_files(tx: &mut Tx<'_>) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> = sql!(
        "discord.list_held_files",
        "SELECT attachment_id, MAX(blake3) FROM discord_attachments \
         WHERE blake3 IS NOT NULL GROUP BY attachment_id"
    )
    .query_as()
    .fetch_all(&mut **tx)
    .await
    .context("list the files held")?;
    Ok(rows.into_iter().collect())
}

pub const SWEEPS: Sweeps = Sweeps::new("discord");
