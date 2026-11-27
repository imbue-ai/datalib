//! The rungs of the Facebook store's migration ladder
//! (`schema_raw::LADDER`).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::BulkUpsertable as _;
use serde_json::Value;
use sqlx::SqliteConnection;

use super::messenger::ThreadFile;
use super::schema_raw::{MediaBlobRow, MESSENGER_MESSAGES_TABLE, MESSENGER_THREADS_TABLE};
use super::{collect_uris, upsert_rows, Record};
use datalib_sql::{sql, sql_id, Sql};

/// A build before the Messenger tables gave every conversation a table
/// of its own, named for its path (`…_messages_inbox_worf_123_message`),
/// holding each `message_<n>.json` as one row. Each such row becomes a
/// thread row and its message rows; each media edge moves to the
/// message that names its `uri`, keeping the bytes it points at; and
/// the old table goes, with the chunk files recorded for it.
pub async fn messenger_tables(conn: &mut SqliteConnection) -> Result<()> {
    'tables: for table in old_conversation_tables(conn).await? {
        let rows: Vec<(String, String)> = Sql::dynamic(
            sql_id!("facebook.migrate.read_messenger_table_rows"),
            format!("SELECT id, json(payload) FROM {table}"),
        )
        .query_as()
        .fetch_all(&mut *conn)
        .await
        .with_context(|| format!("read {table}"))?;

        let mut moved: Vec<(String, Vec<Record>)> = Vec::new();
        for (old_id, payload) in rows {
            let file: Value = serde_json::from_str(&payload)?;
            let Some(at) = file
                .get("thread_path")
                .and_then(Value::as_str)
                .and_then(|path| ThreadFile::of(&format!("messages/{path}/message_1.json")))
            else {
                tracing::warn!(
                    table,
                    "a conversation with no thread_path stays where it is"
                );
                continue 'tables;
            };
            match at.rows(file) {
                Ok(rows) => moved.push((old_id, rows)),
                Err(e) => {
                    tracing::warn!(table, "a conversation stays where it is: {e:#}");
                    continue 'tables;
                }
            }
        }

        for (old_id, new_rows) in &moved {
            let mut by_table: BTreeMap<&str, BTreeMap<String, Value>> = BTreeMap::new();
            for r in new_rows {
                by_table
                    .entry(&r.table)
                    .or_default()
                    .insert(r.id.clone(), r.payload.clone());
            }
            for t in [MESSENGER_THREADS_TABLE, MESSENGER_MESSAGES_TABLE] {
                upsert_rows(conn, t, by_table.get(t).unwrap_or(&BTreeMap::new())).await?;
            }
            move_edges(conn, old_id, new_rows).await?;
        }

        Sql::dynamic(
            sql_id!("facebook.migrate.drop_messenger_table"),
            format!("DROP TABLE {table}"),
        )
        .query()
        .execute(&mut *conn)
        .await
        .with_context(|| format!("drop {table}"))?;
        if table_exists(conn, "ingested_files").await? {
            sql!(
                "facebook.migrate.delete_messenger_ingested_files",
                "DELETE FROM ingested_files WHERE scope = ?"
            )
            .query()
            .bind(format!("facebook/{table}"))
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// The tables an older build made for conversation files: named for a
/// path under `messages/` ending in `message_<n>.json`, with its chunk
/// index dropped.
async fn old_conversation_tables(conn: &mut SqliteConnection) -> Result<Vec<String>> {
    let names: Vec<String> = sql!(
        "facebook.migrate.list_table_names",
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
    )
    .query_scalar()
    .fetch_all(&mut *conn)
    .await?;
    Ok(names
        .into_iter()
        .filter(|n| {
            (n.starts_with("your_facebook_activity_messages_") || n.starts_with("messages_"))
                && n.ends_with("_message")
        })
        .collect())
}

async fn table_exists(conn: &mut SqliteConnection, table: &str) -> Result<bool> {
    let n: i64 = sql!(
        "facebook.migrate.table_exists",
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?"
    )
    .query_scalar()
    .bind(table)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n > 0)
}

/// Re-own the edges of one old conversation row: each goes to every
/// message that names its `uri`, with its bookkeeping, and one that no
/// message names goes.
async fn move_edges(
    conn: &mut SqliteConnection,
    old_owner: &str,
    new_rows: &[Record],
) -> Result<()> {
    let edge_table = MediaBlobRow::TABLE;
    let sidecar = format!("{edge_table}_bookkeeping");
    if !table_exists(conn, edge_table).await? {
        return Ok(());
    }
    let has_sidecar = table_exists(conn, &sidecar).await?;
    let edges: Vec<(String, String, Option<String>)> = sql!(
        "facebook.migrate.list_media_edges_of_owner",
        "SELECT id, uri, blake3 FROM media_blobs WHERE owner_id = ?"
    )
    .query_as()
    .bind(old_owner)
    .fetch_all(&mut *conn)
    .await?;
    for (old_edge, uri, blake3) in edges {
        for Record {
            id: new_owner,
            payload,
            ..
        } in new_rows
        {
            let mut uris = Vec::new();
            collect_uris(payload, &mut uris);
            if !uris.contains(&uri) {
                continue;
            }
            let new_edge = MediaBlobRow::pk_recipe(new_owner, &uri);
            sql!(
                "facebook.migrate.upsert_moved_media_edge",
                "INSERT INTO media_blobs (id, owner_id, uri, blake3) VALUES (?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET blake3 = excluded.blake3"
            )
            .query()
            .bind(&new_edge)
            .bind(new_owner)
            .bind(&uri)
            .bind(&blake3)
            .execute(&mut *conn)
            .await?;
            if has_sidecar {
                // Audited: `sidecar` is the edge table's constant name
                // plus `_bookkeeping`.
                Sql::dynamic(
                    sql_id!("facebook.migrate.copy_media_edge_bookkeeping"),
                    format!(
                        "INSERT OR REPLACE INTO {sidecar} \
                     (id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, \
                      volatile_payload, tz_offset, held_version) \
                     SELECT ?, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, \
                      volatile_payload, tz_offset, held_version FROM {sidecar} WHERE id = ?"
                    ),
                )
                .query()
                .bind(&new_edge)
                .bind(&old_edge)
                .execute(&mut *conn)
                .await?;
            }
        }
        sql!(
            "facebook.migrate.delete_old_media_edge",
            "DELETE FROM media_blobs WHERE id = ?"
        )
        .query()
        .bind(&old_edge)
        .execute(&mut *conn)
        .await?;
        if has_sidecar {
            Sql::dynamic(
                sql_id!("facebook.migrate.delete_old_media_edge_bookkeeping"),
                format!("DELETE FROM {sidecar} WHERE id = ?"),
            )
            .query()
            .bind(&old_edge)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}
