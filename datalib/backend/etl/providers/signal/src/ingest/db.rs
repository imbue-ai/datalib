//! Open + non-DDL data-manipulation for the Signal raw store.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_time::IsoOffsetTimestamp;

use super::backups::{self, Held};
use super::schema_raw::full_ddl;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());

impl RawDb {
    pub async fn read_ledger(&self) -> Result<Vec<Held>> {
        let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT fingerprint, snapshot_dir, ingested_at_utc FROM ingested_backups",
        )
        .fetch_all(self.pool())
        .await
        .context("read ingested_backups")?;
        Ok(rows
            .into_iter()
            .map(|(fingerprint, dir, ingested_at_utc)| Held {
                fingerprint,
                name: dir.and_then(|d| backups::name_of(Path::new(&d))),
                ingested_at_utc,
            })
            .collect())
    }

    /// Stamped again when a snapshot already held is read again, so the
    /// newest stamp names the snapshot HEAD holds.
    pub async fn record_snapshot_ingested(
        &self,
        fingerprint: &str,
        blake3_hex: &str,
        snapshot_dir: &str,
        total_byte_size: u64,
    ) -> Result<()> {
        let (now, tz_offset) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        sqlx::query(
            "INSERT INTO ingested_backups
                 (fingerprint, blake3, snapshot_dir, total_byte_size, ingested_at_utc, tz_offset)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(fingerprint) DO UPDATE SET
                 snapshot_dir = excluded.snapshot_dir,
                 ingested_at_utc = excluded.ingested_at_utc,
                 tz_offset = excluded.tz_offset",
        )
        .bind(fingerprint)
        .bind(blake3_hex)
        .bind(snapshot_dir)
        .bind(total_byte_size as i64)
        .bind(now)
        .bind(tz_offset)
        .execute(self.pool())
        .await
        .context("record_snapshot_ingested")?;
        Ok(())
    }
}
