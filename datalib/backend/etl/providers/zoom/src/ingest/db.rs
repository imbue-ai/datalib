//! Doltlite-backed raw store for the `zoom` provider, and the reads and
//! writes the download makes of it.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use datalib_etl::blob_cas::{BlobCas, CasEdgeRow as _, CasInsert};
use datalib_etl::bulk::{bulk_upsert_first_seen_in_tx, bulk_upsert_in_tx, BulkUpsertable};
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::*;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

pub fn payload(id: &str, value: &Value) -> Result<WirePayload> {
    Ok(WirePayload {
        id: id.to_string(),
        payload: serde_json::to_string(value)?,
    })
}

/// Write the user being copied, its volatile fields in the sidecar.
pub async fn store_user(pool: &sqlx::SqlitePool, id: &str, user: &Value) -> Result<()> {
    let (base, volatile) = dr::split_volatile(user, USER_VOLATILE_PATHS);
    let mut tx = pool.begin().await.context("begin the user")?;
    bulk_upsert_first_seen_in_tx(
        &mut tx,
        &[UserRow {
            id_and_payload: payload(id, &base)?,
        }],
        &IsoOffsetTimestamp::now_local(),
    )
    .await?;
    if let Some(v) = &volatile {
        dr::set_volatile_payloads_in_tx(&mut tx, USERS, &[(id, v)]).await?;
    }
    tx.commit().await.context("commit the user")
}

/// Write one window's listing entries. Stamped first-seen, so an
/// unchanged meeting listed again changes nothing.
pub async fn store_listed<R: BulkUpsertable>(
    tx: &mut Transaction<'static, Sqlite>,
    rows: &[R],
) -> Result<()> {
    bulk_upsert_first_seen_in_tx(tx, rows, &IsoOffsetTimestamp::now_local()).await
}

impl RawDb {
    /// Every stored recording's listing entry, oldest meeting first.
    pub async fn recordings(&self) -> Result<Vec<Value>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT json(payload) FROM recordings ORDER BY start_time, id")
                .fetch_all(self.pool())
                .await
                .context("read the stored recordings")?;
        rows.into_iter()
            .map(|(p,)| serde_json::from_str(&p).context("a recordings payload is not JSON"))
            .collect()
    }

    /// Every meeting either listing named, `(uuid, start_time)`, oldest
    /// first.
    pub async fn meetings(&self) -> Result<Vec<(String, Option<String>)>> {
        sqlx::query_as(
            "SELECT id, MIN(start_time) FROM \
             (SELECT id, start_time FROM recordings \
              UNION ALL SELECT id, start_time FROM reported_meetings) \
             GROUP BY id ORDER BY 2, 1",
        )
        .fetch_all(self.pool())
        .await
        .context("read the stored meetings")
    }

    /// Every listed summary, `(uuid, last_modified)`.
    pub async fn summary_listings(&self) -> Result<Vec<(String, Option<String>)>> {
        sqlx::query_as("SELECT id, last_modified FROM summary_listings ORDER BY id")
            .fetch_all(self.pool())
            .await
            .context("read the stored summary listings")
    }
}

/// A recording file as fetched: `None` bytes for one Zoom no longer has.
#[derive(Debug, Clone)]
pub struct FetchedFile {
    pub meeting_uuid: String,
    pub file_key: String,
    pub content_type: Option<&'static str>,
    pub bytes: Option<Vec<u8>>,
}

impl FetchedFile {
    pub fn key(&self) -> String {
        RecordingFileRow::pk_recipe(&self.meeting_uuid, &self.file_key)
    }
}

/// Write a flush of files: their bytes into the CAS first, which
/// commits on its own, then the edges naming them.
pub async fn store_files(
    tx: &mut Transaction<'static, Sqlite>,
    cas: &BlobCas,
    files: &[&FetchedFile],
) -> Result<()> {
    let inserts: Vec<CasInsert<'_, String>> = files
        .iter()
        .filter_map(|f| {
            Some(CasInsert {
                id: f.key(),
                bytes: f.bytes.as_deref()?,
                content_type: f.content_type,
            })
        })
        .collect();
    let stored = cas.put_many(inserts).await?;
    let edges: Vec<RecordingFileRow> = files
        .iter()
        .map(|f| RecordingFileRow {
            id: f.key(),
            meeting_uuid: f.meeting_uuid.clone(),
            file_key: f.file_key.clone(),
            blake3: stored.get(&f.key()).cloned(),
        })
        .collect();
    bulk_upsert_in_tx(tx, &edges, &IsoOffsetTimestamp::now_local()).await
}

/// Write fetched rows of a table whose rows are whole answers
/// (`participants`, `summaries`).
pub async fn store_rows<R: BulkUpsertable>(
    tx: &mut Transaction<'static, Sqlite>,
    rows: &[R],
) -> Result<()> {
    bulk_upsert_in_tx(tx, rows, &IsoOffsetTimestamp::now_local()).await
}
