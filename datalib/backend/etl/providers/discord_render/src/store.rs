//! The raw store open for one render: what every conversation looks up,
//! read whole, and the conversations themselves a page at a time, their
//! attachment bytes a batch at a time.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle, CasEdgeRow};
use datalib_etl::doltlite_raw::Reader;
use datalib_etl_chat_common::Page;
use datalib_etl_discord::ingest::db::{db_path_for, files_of, id_of};
use datalib_etl_discord::ingest::schema_raw::{
    is_thread, split_message_key, DiscordAttachmentRow, ACCOUNT, ATTACHMENTS, CHANNELS, GUILDS,
    MESSAGES, ROLES, USERS,
};
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_etl_render::pages::{block_on, KeyColumn, RenderUnitPages};
use datalib_etl_render::processor::Unparsed;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::ids;
use datalib_sql::{sql, sql_id, Sql};

/// Maps a file's id to its bytes in the CAS, for a batch of conversations.
pub const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT attachment_id AS ref_id, MAX(blake3) AS blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM discord_attachments
     WHERE attachment_id IN ({placeholders}) AND blake3 IS NOT NULL
     GROUP BY attachment_id";

const UNIT_KEY: KeyColumn = KeyColumn {
    table: MESSAGES,
    column: "channel_id",
};

#[derive(Debug, Clone)]
pub struct User {
    pub username: Option<String>,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
    pub bot: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub id: String,
    pub name: Option<String>,
    pub guild_id: Option<String>,
    pub parent_id: Option<String>,
    pub is_thread: bool,
    /// Who else is in a direct message.
    pub recipients: Vec<String>,
}

/// What every conversation looks up.
#[derive(Debug, Default)]
pub struct Shared {
    /// The signed-in account's id, and what the Account column shows.
    pub account: Option<(String, Option<String>)>,
    pub users: BTreeMap<String, User>,
    pub labels: BTreeMap<String, String>,
    pub channels: BTreeMap<String, Channel>,
    pub channel_names: BTreeMap<String, String>,
    pub guilds: BTreeMap<String, String>,
    pub roles: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub key: String,
    pub id: String,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct Conversation {
    pub channel_id: String,
    /// Oldest first.
    pub messages: Vec<Message>,
    pub blobs: BlobBundle,
    pub inputs: Inputs,
}

impl Conversation {
    pub fn attachment_bytes(&self) -> u64 {
        self.messages
            .iter()
            .flat_map(|m| files_of(&m.raw))
            .filter_map(|f| f.get("size").and_then(Value::as_u64))
            .sum()
    }
}

pub struct Store {
    reader: Reader,
    cas: Option<blob_cas::BlobCas>,
    pub shared: Shared,
    /// The conversations to render, or `None` for every one.
    pub render: Option<HashSet<String>>,
    pub new_head: Option<String>,
    pub conversations: usize,
    pub unparsed: Vec<Unparsed>,
}

/// The store under `path`, open at the driver's commit. `None` when there
/// is none, or nothing is committed in it.
pub fn open(path: &Path, source_id: &str, range: RawRange<'_>) -> Result<Option<Store>> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        return Ok(None);
    }
    block_on(open_async(&db_path, source_id, range))
}

/// The conversations with a message or a file that changed.
const CHANGED_SQL: &str = "
    SELECT DISTINCT channel_id FROM (
        SELECT coalesce(to_channel_id, from_channel_id) AS channel_id
          FROM dolt_diff_messages
         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
        UNION
        SELECT m.channel_id
          FROM dolt_diff_discord_attachments d
          JOIN messages m ON m.id = coalesce(d.to_message_key, d.from_message_key)
         WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged')
     WHERE channel_id IS NOT NULL";

/// Every document names the account it was copied as.
const ACCOUNT_CHANGED_SQL: &str = "
    SELECT 1 FROM dolt_diff_account
     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged' LIMIT 1";

async fn open_async(db_path: &Path, source_id: &str, range: RawRange<'_>) -> Result<Option<Store>> {
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open discord doltlite for render {}", db_path.display()))?
    else {
        return Ok(None);
    };
    let pool = reader.pool().clone();
    let pin = reader.pin().clone();
    let scan = datalib_etl::doltlite_raw::scan_render_units(
        &pool,
        range.cursor,
        &pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            fanout_query: Some(ACCOUNT_CHANGED_SQL),
            render_unit_query: CHANGED_SQL,
        },
    )
    .await?;
    let all = conversation_ids(&pool).await?;
    // Every one there is, and those there no longer are: each has to be
    // looked at for its document to go.
    let forward = match (scan.render, scan.changed_render_units) {
        (Some(render), _) => Some(render),
        (None, Some(changed)) => Some(all.iter().cloned().chain(changed).collect()),
        (None, None) => None,
    };
    // The driver names a stale conversation by its document's id.
    let by_uuid: BTreeMap<String, String> = match (&range.stale, &forward) {
        (Some(_), Some(_)) => all
            .iter()
            .map(|id| (ids::conversation(source_id, id).uuid, id.clone()))
            .collect(),
        _ => BTreeMap::new(),
    };
    let narrowed = range.narrow_by(forward.as_ref(), |uuid| by_uuid.get(uuid).cloned());
    let mut unparsed = Vec::new();
    let shared = shared(&pool, &mut unparsed).await?;
    let cas = blob_cas::open_cas_for_render(db_path)
        .await
        .with_context(|| format!("open the blob store beside {}", db_path.display()))?;
    let conversations = narrowed.render.as_ref().map_or(all.len(), HashSet::len);
    Ok(Some(Store {
        reader,
        cas,
        shared,
        render: narrowed.render,
        new_head: scan.new_head,
        conversations,
        unparsed,
    }))
}

/// Each conversation, found by a seek past the one before it: there are few
/// beside the messages, and this runs on every render.
const CONVERSATIONS_SQL: Sql = sql!(
    "discord_render.list_conversations",
    "
    WITH RECURSIVE c(id) AS (
        SELECT min(channel_id) FROM messages INDEXED BY messages_by_channel
        UNION ALL
        SELECT (SELECT min(channel_id) FROM messages INDEXED BY messages_by_channel
                 WHERE channel_id > c.id)
          FROM c WHERE c.id IS NOT NULL)
    SELECT id FROM c WHERE id IS NOT NULL"
);

async fn conversation_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    CONVERSATIONS_SQL
        .query_scalar()
        .fetch_all(pool)
        .await
        .context("list conversations")
}

/// Every `(id, payload)` of one table.
async fn rows(
    pool: &SqlitePool,
    table: &'static str,
    unparsed: &mut Vec<Unparsed>,
) -> Result<Vec<(String, Value)>> {
    // Audited: `table` is one of the store's own names.
    let sql = Sql::dynamic(
        sql_id!("discord_render.list_payloads"),
        format!("SELECT id, json(payload) FROM {table} WHERE payload IS NOT NULL ORDER BY id"),
    );
    let raw: Vec<(String, String)> = sql
        .query_as()
        .fetch_all(pool)
        .await
        .with_context(|| format!("read {table}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for (id, payload) in raw {
        match serde_json::from_str::<Value>(&payload) {
            Ok(v) => out.push((id, v)),
            Err(_) => unparsed.push(Unparsed::new(table, &id, &payload)),
        }
    }
    Ok(out)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
}

async fn shared(pool: &SqlitePool, unparsed: &mut Vec<Unparsed>) -> Result<Shared> {
    let mut out = Shared::default();
    for (id, v) in rows(pool, ACCOUNT, unparsed).await? {
        let name = datalib_etl_discord::user_label(
            text(&v, "global_name").as_deref(),
            text(&v, "username").as_deref(),
            &id,
        );
        let label =
            datalib_etl_chat_common::account_label(&id, text(&v, "email").as_deref(), Some(&name));
        out.account = Some((id, label));
    }
    for (id, v) in rows(pool, USERS, unparsed).await? {
        let user = User {
            username: text(&v, "username"),
            global_name: text(&v, "global_name"),
            avatar: text(&v, "avatar"),
            bot: v.get("bot").and_then(Value::as_bool) == Some(true),
        };
        let label = datalib_etl_discord::user_label(
            user.global_name.as_deref(),
            user.username.as_deref(),
            &id,
        );
        out.labels.insert(id.clone(), label);
        out.users.insert(id, user);
    }
    for (id, v) in rows(pool, CHANNELS, unparsed).await? {
        let recipients = v
            .get("recipients")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| id_of(r).map(String::from))
            .collect();
        let name = text(&v, "name");
        out.channel_names
            .extend(name.clone().map(|n| (id.clone(), n)));
        out.channels.insert(
            id.clone(),
            Channel {
                id,
                name,
                guild_id: text(&v, "guild_id"),
                parent_id: text(&v, "parent_id"),
                is_thread: is_thread(&v),
                recipients,
            },
        );
    }
    for (id, v) in rows(pool, GUILDS, unparsed).await? {
        out.guilds.extend(text(&v, "name").map(|n| (id, n)));
    }
    for (id, v) in rows(pool, ROLES, unparsed).await? {
        out.roles.extend(text(&v, "name").map(|n| (id, n)));
    }
    Ok(out)
}

impl Store {
    /// The conversations to render, `page` at a time.
    pub fn pages(&self, page: usize) -> RenderUnitPages {
        match &self.render {
            None => RenderUnitPages::walk(UNIT_KEY, page),
            Some(ids) => RenderUnitPages::listed(ids.iter().cloned(), page),
        }
    }

    /// The next page's conversations, and, on a narrowed run, every one it
    /// looked at, by channel id.
    pub fn next_page(&self, pages: &mut RenderUnitPages) -> Result<Option<Page<Conversation>>> {
        block_on(async {
            let pool = self.reader.pool();
            let Some(keys) = pages.next(pool).await? else {
                return Ok(None);
            };
            let mut unparsed = Vec::new();
            let conversations = self.load(pool, &keys, &mut unparsed).await?;
            Ok(Some(Page {
                conversations,
                looked_at: match self.render {
                    None => Vec::new(),
                    Some(_) => keys,
                },
                unparsed,
            }))
        })
    }

    async fn load(
        &self,
        pool: &SqlitePool,
        keys: &[String],
        unparsed: &mut Vec<Unparsed>,
    ) -> Result<Vec<Conversation>> {
        let rows: Vec<(String, String, String)> = sql!(
            "discord_render.list_messages_of_conversations",
            "SELECT id, channel_id, json(payload) FROM messages INDEXED BY messages_by_channel \
             WHERE channel_id IN (SELECT value FROM json_each(?)) AND payload IS NOT NULL \
             ORDER BY channel_id, id"
        )
        .query_as()
        .bind(serde_json::to_string(keys)?)
        .fetch_all(pool)
        .await
        .context("read a page of conversations")?;
        let mut out: BTreeMap<String, (Vec<Message>, Inputs)> = BTreeMap::new();
        for (key, channel_id, payload) in rows {
            let Ok(raw) = serde_json::from_str::<Value>(&payload) else {
                unparsed.push(Unparsed::new(MESSAGES, &key, &payload));
                continue;
            };
            let Some((_, id)) = split_message_key(&key) else {
                continue;
            };
            let (messages, inputs) = out.entry(channel_id).or_default();
            inputs.read(MESSAGES, &key);
            for file in files_of(&raw).into_iter().filter_map(id_of) {
                inputs.read(ATTACHMENTS, &DiscordAttachmentRow::pk_recipe(&key, file));
            }
            messages.push(Message { key, id, raw });
        }
        Ok(out
            .into_iter()
            .map(|(channel_id, (messages, inputs))| {
                if let Some((me, _)) = &self.shared.account {
                    inputs.read(ACCOUNT, me);
                }
                Conversation {
                    channel_id,
                    messages,
                    blobs: BlobBundle::default(),
                    inputs,
                }
            })
            .collect())
    }

    /// Copies of `batch` with their attachments' blobs loaded.
    pub fn load_blobs(&self, batch: &[&Conversation]) -> Result<Vec<Conversation>> {
        block_on(async {
            let mut out: Vec<Conversation> = batch.iter().map(|c| (*c).clone()).collect();
            let refs = out.iter().enumerate().map(|(i, c)| {
                let ids: Vec<String> = c
                    .messages
                    .iter()
                    .flat_map(|m| files_of(&m.raw))
                    .filter_map(|f| id_of(f).map(String::from))
                    .collect();
                (i, ids)
            });
            let mut blobs = BlobBundle::load_many(
                self.reader.pool(),
                self.cas.as_ref(),
                ATTACHMENTS_PROJECTION_SQL,
                refs,
            )
            .await?;
            for (i, c) in out.iter_mut().enumerate() {
                if let Some(b) = blobs.remove(&i) {
                    c.blobs = b;
                }
            }
            Ok(out)
        })
    }

    pub fn close(self) {
        block_on(async {
            self.reader.close().await;
            if let Some(cas) = self.cas {
                cas.close().await;
            }
        })
    }
}
