//! Doltlite-backed raw store for the `markdown` provider.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::Row;

use datalib_etl::bulk::{bulk_upsert_first_seen_in_tx, bulk_upsert_in_tx};
use datalib_etl_files::file_changes_scan::ScannedFile;
use datalib_etl_files::file_checkpoint;
use datalib_problems::{Outcome, Problem};
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::{full_ddl, MarkdownFileRow, MarkdownScanMetaRow, MARKDOWN_FILES};
use datalib_sql::sql;

pub fn db_path_for(raw_dir: &Path) -> PathBuf {
    datalib_etl::raw_layout::entities_db(raw_dir)
}

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

impl RawDb {
    /// Record where this scan ran. A root that did not move commits
    /// nothing.
    pub async fn write_scan_meta(
        &self,
        row: &MarkdownScanMetaRow,
        now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin scan_meta tx")?;
        bulk_upsert_first_seen_in_tx(&mut tx, std::slice::from_ref(row), now)
            .await
            .context("upsert markdown_scan_meta")?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }

    /// Store one file's note and stamp the file as read, in one
    /// transaction: a crash between the two cannot leave a stamp claiming
    /// a note that never landed. `problem` is what this read could not
    /// use, kept until the file is read again.
    pub async fn write_file(
        &self,
        row: &MarkdownFileRow,
        scope: &str,
        file: &ScannedFile,
        problem: Option<(Outcome, Problem)>,
        now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin note tx")?;
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(row), now)
            .await
            .context("upsert markdown_files")?;
        file_checkpoint::record_file_with_problem(&mut tx, scope, file, problem).await?;
        tx.commit().await.context("commit note tx")?;
        Ok(())
    }

    /// Delete the note of a file that is gone, with its sidecar row, its
    /// problems and its row in `ingested_files` — in one transaction, so a
    /// crash leaves the file stamped and the next run retries.
    pub async fn delete_file(&self, scope: &str, rel: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin delete tx")?;
        for sql in [
            sql!(
                "markdown.delete_file.delete_markdown_files",
                "DELETE FROM markdown_files WHERE id = ?"
            ),
            sql!(
                "markdown.delete_file.delete_markdown_files_bookkeeping",
                "DELETE FROM markdown_files_bookkeeping WHERE id = ?"
            ),
        ] {
            sql.query()
                .bind(rel)
                .execute(&mut *tx)
                .await
                .context("delete a note")?;
        }
        datalib_etl::prune::forget_record_problems_in_tx(&mut tx, MARKDOWN_FILES, rel).await?;
        file_checkpoint::forget_file(&mut tx, scope, rel).await?;
        tx.commit().await.context("commit delete tx")?;
        Ok(())
    }

    /// The scan's root and the `markdown_scan_meta` row it came from — a
    /// row every rendered note reads, since its link is an absolute path
    /// under that root.
    pub async fn scan_root(&self) -> Result<Option<(String, PathBuf)>> {
        let row = sql!(
            "markdown.read_scan_root",
            "SELECT id, abs_root FROM markdown_scan_meta ORDER BY id LIMIT 1"
        )
        .query()
        .fetch_optional(self.pool())
        .await
        .context("read markdown_scan_meta")?;
        Ok(row.map(|r| {
            (
                r.get::<String, _>("id"),
                PathBuf::from(r.get::<String, _>("abs_root")),
            )
        }))
    }

    /// Every stored note, or the ones `only` names, in path order.
    pub async fn notes(&self, only: Option<&HashSet<String>>) -> Result<Vec<MarkdownFileRow>> {
        let rows = sql!(
            "markdown.list_notes",
            "SELECT id, blake3, size, modified_at, text FROM markdown_files ORDER BY id"
        )
        .query()
        .fetch_all(self.pool())
        .await
        .context("read markdown_files")?;
        Ok(rows
            .into_iter()
            .map(|r| MarkdownFileRow {
                id: r.get("id"),
                blake3: r.get("blake3"),
                size: r.get("size"),
                modified_at: r.get("modified_at"),
                text: r.get("text"),
            })
            .filter(|row| only.is_none_or(|ids| ids.contains(&row.id)))
            .collect())
    }
}
