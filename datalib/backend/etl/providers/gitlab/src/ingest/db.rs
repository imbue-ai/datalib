//! Doltlite-backed raw store for the GitLab provider.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl_forge_ingest_common::{load_self_identity, prune_children, row_payload};

use super::canonicalize::canonicalize_payload;
use super::schema_raw::{
    full_ddl, DiscussionRow, MergeRequestRow, SelfIdentityRow, LADDER, SELF_IDENTITY_VOLATILE_PATHS,
};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl(), LADDER);

impl RawDb {
    // ── self_identity ───────────────────────────────────────────────

    pub async fn upsert_self_identity(&self, payload: &Value) -> Result<()> {
        let payload = &canonicalize_payload(payload);
        // The clock reading goes to the sidecar, or this row differs on
        // every sync (see `SELF_IDENTITY_VOLATILE_PATHS`).
        let (base, volatile) = dr::split_volatile(payload, SELF_IDENTITY_VOLATILE_PATHS);
        let row = SelfIdentityRow::from_payload(&base)?;
        let id = row.id_and_payload.id.clone();
        let volatile_pairs: Vec<(&str, &Value)> =
            volatile.iter().map(|v| (id.as_str(), v)).collect();
        // No event tape on this store; the split is what this call is
        // for.
        dr::bulk_upsert_with_tape_split(
            self.pool(),
            None,
            &[row],
            &[(id.as_str(), payload)],
            &volatile_pairs,
        )
        .await
    }

    pub async fn load_self_identity(&self) -> Result<Option<Value>> {
        load_self_identity(self.pool()).await
    }

    // ── merge_requests ──────────────────────────────────────────────

    /// The record alone: its sidecar is written by the fetch loop, in
    /// the same transaction, with the version it is held at.
    pub async fn store_merge_request(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        proj: &str,
        iid: u32,
        payload: &Value,
    ) -> Result<()> {
        let row = MergeRequestRow::from_payload(proj, iid, &canonicalize_payload(payload))?;
        bulk_upsert_entity_in_tx(tx, &[row]).await
    }

    // ── discussions ─────────────────────────────────────────────────

    /// Every discussion of one MR.
    pub async fn store_discussions(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        proj: &str,
        iid: u32,
        payloads: &[Value],
        now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        let rows: Vec<DiscussionRow> = payloads
            .iter()
            .map(|p| DiscussionRow::from_payload(proj, iid, &canonicalize_payload(p)))
            .collect::<Result<Vec<_>>>()?;
        bulk_upsert_in_tx(tx, &rows, now).await
    }

    // ── loads ───────────────────────────────────────────────────────

    pub async fn load_merge_requests(&self) -> Result<Vec<LoadedMergeRequest>> {
        let rows = sqlx::query(
            "SELECT id, project_full_path, mr_iid, json(payload) AS payload
             FROM merge_requests WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .context("select merge_requests")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let Some(payload) = row_payload(&r) else {
                continue;
            };
            out.push(LoadedMergeRequest {
                id: r.try_get("id").unwrap_or_default(),
                project_full_path: r.try_get("project_full_path").unwrap_or_default(),
                mr_iid: r.try_get::<i64, _>("mr_iid").unwrap_or(0) as u32,
                payload,
            });
        }
        Ok(out)
    }

    pub async fn load_discussions(&self) -> Result<Vec<LoadedDiscussion>> {
        let rows = sqlx::query(
            "SELECT id, project_full_path, mr_iid, discussion_id, json(payload) AS payload
             FROM discussions WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .context("select discussions")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let Some(payload) = row_payload(&r) else {
                continue;
            };
            out.push(LoadedDiscussion {
                id: r.try_get("id").unwrap_or_default(),
                project_full_path: r.try_get("project_full_path").unwrap_or_default(),
                mr_iid: r.try_get::<i64, _>("mr_iid").unwrap_or(0) as u32,
                discussion_id: r.try_get("discussion_id").unwrap_or_default(),
                payload,
            });
        }
        Ok(out)
    }

    /// Drop this MR's discussion rows that the fresh listing did not name.
    pub async fn prune_mr_discussions(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        proj: &str,
        iid: u32,
        listed: &[Value],
    ) -> Result<usize> {
        let keep: HashSet<String> = listed
            .iter()
            .filter_map(|d| d.get("id").and_then(|v| v.as_str()))
            .map(|id| super::schema_raw::discussion_pk_recipe(proj, iid, id))
            .collect();
        let iid = iid.to_string();
        prune_children(
            tx,
            "discussions",
            &[("project_full_path", proj), ("mr_iid", &iid)],
            &keep,
        )
        .await
    }

    pub async fn any_merge_requests(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM merge_requests WHERE payload IS NOT NULL LIMIT 1")
            .fetch_optional(self.pool())
            .await
            .context("any_merge_requests")?;
        Ok(row.is_some())
    }
}

#[derive(Debug, Clone)]
pub struct LoadedMergeRequest {
    pub id: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct LoadedDiscussion {
    pub id: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub discussion_id: String,
    pub payload: Value,
}

#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub self_identity: Option<Value>,
    pub merge_requests: Vec<LoadedMergeRequest>,
    pub discussions: Vec<LoadedDiscussion>,
}

pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                self_identity: db.load_self_identity().await?,
                merge_requests: db.load_merge_requests().await?,
                discussions: db.load_discussions().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn self_identity_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_self_identity(
            &json!({"id": 7, "username": "tt", "web_url": "https://gitlab.com/tt"}),
        )
        .await
        .unwrap();
        let me = db.load_self_identity().await.unwrap().expect("self");
        assert_eq!(me["id"], 7);
        assert_eq!(me["username"], "tt");
    }

    #[tokio::test]
    async fn mr_and_discussion_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        db.store_merge_request(
            &mut tx,
            "ns/proj",
            12,
            &json!({
                "iid": 12,
                "web_url": "https://gitlab.com/ns/proj/-/merge_requests/12",
                "state": "opened",
                "source_branch": "feat",
                "target_branch": "main",
            }),
        )
        .await
        .unwrap();
        db.store_discussions(
            &mut tx,
            "ns/proj",
            12,
            &[json!({"id": "abc", "individual_note": false, "notes": [{"updated_at": "2025-01-01T00:00:00Z"}]})],
            &IsoOffsetTimestamp::now_local(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mrs = db.load_merge_requests().await.unwrap();
        assert_eq!(mrs.len(), 1);
        assert_eq!(mrs[0].mr_iid, 12);
        let ds = db.load_discussions().await.unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].discussion_id, "abc");
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        db.store_merge_request(
            &mut tx,
            "ns/proj",
            12,
            &json!({"iid": 12, "state": "opened"}),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let row =
            sqlx::query("SELECT typeof(payload) AS t FROM merge_requests WHERE id='ns/proj!12'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }
}
