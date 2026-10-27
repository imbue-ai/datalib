//! Whether a source that reads its whole input or none of it would read
//! anything new: every input file and the settings the read depends on,
//! compared with what its last complete read saw. Kept in the store's
//! `ingested_files` under one scope; README.md § "Skipping a whole read".

use anyhow::{Context, Result};
use sqlx::SqlitePool;

use crate::file_checkpoint;
use crate::fsscan::{FileScanCursor, Scan, ScannedFile};

/// The settings' row. No scanned file has an empty `rel`.
const SETTINGS_REL: &str = "";

pub struct ReadStamp {
    scope: &'static str,
    rows: Vec<ScannedFile>,
}

impl ReadStamp {
    /// `None` when the scan could not see every file: an input part of
    /// which cannot be read is never the one last read.
    pub fn of(scope: &'static str, scan: &Scan, settings: &str) -> Option<Self> {
        if !scan.errors.is_empty() || !scan.unreadable.is_empty() {
            return None;
        }
        let mut rows = scan.files.clone();
        rows.push(ScannedFile {
            path: Default::default(),
            rel: SETTINGS_REL.to_string(),
            size: settings.len() as i64,
            blake3: *blake3::hash(settings.as_bytes()).as_bytes(),
        });
        Some(Self { scope, rows })
    }

    /// Whether the last complete read saw exactly these files with these
    /// settings.
    pub async fn is_current(&self, pool: &SqlitePool) -> Result<bool> {
        let read = file_checkpoint::load_cursor(pool, self.scope).await?;
        let now: FileScanCursor = self
            .rows
            .iter()
            .map(|f| (f.rel.clone(), f.blake3))
            .collect();
        Ok(read == now)
    }

    /// Call only after a read that finished: stopped, or with a file or a
    /// record it could not read, it would vouch for what it never stored.
    pub async fn record(&self, pool: &SqlitePool) -> Result<()> {
        let held = file_checkpoint::load_cursor(pool, self.scope).await?;
        let mut tx = pool.begin().await.context("begin read stamp tx")?;
        for rel in held.keys() {
            if !self.rows.iter().any(|f| &f.rel == rel) {
                file_checkpoint::forget_file(&mut tx, self.scope, rel).await?;
            }
        }
        for f in &self.rows {
            file_checkpoint::record_file(&mut tx, self.scope, f).await?;
        }
        tx.commit().await.context("commit read stamp tx")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint_cache::FingerprintCache;
    use crate::fsscan::{self, ScanOptions};
    use std::path::Path;

    const SCOPE: &str = "test/read";

    async fn pool(dir: &Path) -> SqlitePool {
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("store.sqlite"))
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(datalib_etl::doltlite_raw::PROBLEMS_DDL)
            .execute(&pool)
            .await
            .unwrap();
        pool
    }

    async fn stamp(cache: &FingerprintCache, input: &Path, settings: &str) -> ReadStamp {
        let scan = fsscan::scan(cache, input, &ScanOptions::default(), |_| true)
            .await
            .unwrap();
        ReadStamp::of(SCOPE, &scan, settings).unwrap()
    }

    /// Current only once recorded, and only for the same bytes, the same
    /// files and the same settings.
    #[tokio::test]
    async fn current_means_same_files_same_bytes_same_settings() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("in");
        std::fs::create_dir(&input).unwrap();
        std::fs::write(input.join("a.mbox"), "one").unwrap();
        let cache = FingerprintCache::open(&work.path().join("cache.sqlite"))
            .await
            .unwrap();
        let pool = pool(work.path()).await;

        let first = stamp(&cache, &input, "labels=[]").await;
        assert!(!first.is_current(&pool).await.unwrap(), "never read");
        first.record(&pool).await.unwrap();
        assert!(stamp(&cache, &input, "labels=[]")
            .await
            .is_current(&pool)
            .await
            .unwrap());
        assert!(
            !stamp(&cache, &input, "labels=[Inbox]")
                .await
                .is_current(&pool)
                .await
                .unwrap(),
            "settings changed"
        );

        std::fs::write(input.join("b.mbox"), "two").unwrap();
        let added = stamp(&cache, &input, "labels=[]").await;
        assert!(!added.is_current(&pool).await.unwrap(), "a file added");
        added.record(&pool).await.unwrap();

        std::fs::remove_file(input.join("b.mbox")).unwrap();
        let removed = stamp(&cache, &input, "labels=[]").await;
        assert!(!removed.is_current(&pool).await.unwrap(), "a file removed");
        removed.record(&pool).await.unwrap();
        assert!(removed.is_current(&pool).await.unwrap());

        std::fs::write(input.join("a.mbox"), "ONE").unwrap();
        assert!(
            !stamp(&cache, &input, "labels=[]")
                .await
                .is_current(&pool)
                .await
                .unwrap(),
            "a file rewritten"
        );
    }
}
