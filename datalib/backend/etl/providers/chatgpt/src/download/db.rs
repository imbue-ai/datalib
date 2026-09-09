//! Doltlite-backed raw store for the ChatGPT provider.

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::blob_cas::BlobCas;
use datalib_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES};

pub use datalib_etl::doltlite_raw::db_path_for;

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&datalib_etl::blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    /// Release every store this handle opened, and wait for the
    /// connections to go away. Dropping only schedules that.
    pub async fn close(self) {
        self.close_all().await;
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Reset bytes-have-been-fetched state for `refetch_blobs`: clear
    /// the per-provider `blake3` column on `chatgpt_attachments` so
    /// the next walk re-decodes and re-stores. The CAS bytes
    /// themselves stay — `put_many` is INSERT OR IGNORE, re-hashing
    /// the same bytes lands on the same blake3.
    pub async fn clear_blob_hashes(&self) -> Result<()> {
        sqlx::query("UPDATE chatgpt_attachments SET blake3 = NULL")
            .execute(&self.pool)
            .await
            .context("clear chatgpt_attachments.blake3")?;
        Ok(())
    }

    // ── `me` ────────────────────────────────────────────────────────

    pub async fn load_me(&self) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT json(payload) AS payload FROM me ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("select me")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s: String| serde_json::from_str(&s).ok()))
    }

    // ── conversations: listing skip-check ──────────────────────────

    /// Bulk-read `(id → update_time)` for the listed ids. Returns one
    /// entry per *existing* row (with a non-null `update_time`). Missing
    /// ids are absent from the map — caller treats them as "we don't
    /// have this conversation yet, fetch it." Used by the listing pass
    /// to decide which conversations need a detail fetch.
    pub async fn existing_update_times(&self, ids: &[&str]) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, update_time FROM conversations \
              WHERE id IN ({placeholders}) AND update_time IS NOT NULL"
        );
        // Audited: static template; the only interpolation is a `?,?,?` run sized
        // from the chunk length. Every value is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in ids {
            q = q.bind(*id);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .context("existing_update_times")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").unwrap_or_default();
            if let Ok(ut) = r.try_get::<String, _>("update_time") {
                out.insert(id, ut);
            }
        }
        Ok(out)
    }

    /// Delete every conversation not in `keep`, and its attachment edges.
    ///
    /// Only for a caller holding a **complete** listing — see the gate at
    /// the callsite. That gate is the whole safety story: nothing here
    /// second-guesses how much it deletes, because the rows stay in
    /// doltlite history either way.
    pub async fn prune_conversations(&self, keep: &HashSet<String>) -> Result<usize> {
        let held: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations")
            .fetch_all(&self.pool)
            .await
            .context("list conversation ids for prune")?;
        let gone: Vec<String> = held
            .iter()
            .filter(|id| !keep.contains(*id))
            .cloned()
            .collect();
        if gone.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await.context("begin prune tx")?;
        for chunk in gone.chunks(datalib_etl::bulk::SQL_CHUNK) {
            let mut placeholders = String::new();
            datalib_etl::bulk::push_placeholder_list(&mut placeholders, chunk.len());
            for sql in [
                format!(
                    "DELETE FROM chatgpt_attachments WHERE conversation_id IN ({placeholders})"
                ),
                format!("DELETE FROM conversations WHERE id IN ({placeholders})"),
                format!("DELETE FROM conversations_bookkeeping WHERE id IN ({placeholders})"),
            ] {
                // Audited: static table names; the IN-list is a `?,?,?` run
                // sized from the chunk and every id is bound.
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for id in chunk {
                    q = q.bind(id.clone());
                }
                q.execute(&mut *tx)
                    .await
                    .context("prune chatgpt conversations")?;
            }
        }
        tx.commit().await.context("commit prune tx")?;
        datalib_etl::prune::record("chatgpt conversations", held.len(), gone.len());
        Ok(gone.len())
    }

    pub async fn record_conversation_error(&self, id: &str, err: &str) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin record_conversation_error tx")?;
        dr::record_object_error(&mut tx, "conversations", id, err).await?;
        tx.commit()
            .await
            .context("commit record_conversation_error tx")?;
        Ok(())
    }

    pub async fn failed_conversation_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(&self.pool, "conversations").await
    }

    /// Snapshot `(file_id → blake3)` for every attachment whose bytes
    /// have ever landed in the CAS. Loaded once at the start of a
    /// fetch run; updated in-place as new downloads land. Replaces
    /// the per-file SQL `attachment_has_bytes` lookup.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        datalib_etl::blob_cas::load_blake3_index(&self.pool, "chatgpt_attachments", "file_id").await
    }

    // ── loads ───────────────────────────────────────────────────────

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT c.id, json(c.payload) AS payload, b.fetched_at
             FROM conversations c
             LEFT JOIN conversations_bookkeeping b ON b.id = c.id
             WHERE c.payload IS NOT NULL
             ORDER BY c.id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select conversations")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fetched_at: Option<String> = r.try_get("fetched_at").ok();
            let Ok(payload_v) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            out.push(LoadedConversation {
                id,
                payload: payload_v,
                fetched_at,
            });
        }
        Ok(out)
    }
}

/// One row's worth of loaded conversation data — payload plus the
/// fetch timestamp. Rows only exist post-detail-fetch.
#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub payload: Value,
    pub fetched_at: Option<String>,
}

/// Bag returned to the synchronous render / synthesize path.
/// Attachment bytes are no longer carried alongside — render's
/// `parse` loads a per-doc [`BlobBundle`] for each conversation,
/// keeping render fully sync.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub me: Option<Value>,
    pub conversations: Vec<LoadedConversation>,
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production render uses
/// `datalib_etl_chatgpt_render::render::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything. Attachment bytes
/// are NOT loaded here — tests that need them load a [`BlobBundle`]
/// via `BlobBundle::load(...)` directly.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                me: db.load_me().await?,
                conversations: db.load_conversations().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn me_round_trips() {
        use crate::download::schema_raw::MeRow;
        use datalib_etl::bulk::bulk_upsert_in_tx;
        use datalib_etl::doltlite_raw::WirePayload;
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let me = json!({"id": "u1", "email": "x@y", "name": "X Y"});
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[MeRow {
                id_and_payload: WirePayload {
                    id: "u1".into(),
                    payload: serde_json::to_string(&me).unwrap(),
                },
                email: Some("x@y".into()),
                name: Some("X Y".into()),
            }],
            "2026-06-11T00:00:00-07:00",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let loaded = db.load_me().await.unwrap().expect("me present");
        assert_eq!(loaded["id"], "u1");
        assert_eq!(loaded["email"], "x@y");
    }

    #[tokio::test]
    async fn existing_update_times_round_trips() {
        use crate::download::schema_raw::ConversationRow;
        use datalib_etl::bulk::bulk_upsert_in_tx;
        use datalib_etl::doltlite_raw::WirePayload;
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[ConversationRow {
                id_and_payload: WirePayload {
                    id: "c1".into(),
                    payload: serde_json::to_string(&json!({"id":"c1","mapping":{}})).unwrap(),
                },
                title: Some("T".into()),
                // Stored as the detail endpoint's JSON-encoded float;
                // this test only checks the storage round-trip, not the
                // cross-shape comparison (see download::update_time_secs).
                update_time: Some("1.0".into()),
            }],
            "2026-06-11T00:00:00-07:00",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let map = db.existing_update_times(&["c1", "missing"]).await.unwrap();
        assert_eq!(map.get("c1").map(String::as_str), Some("1.0"));
        assert!(!map.contains_key("missing"));
    }
}
