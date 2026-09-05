//! Doltlite-backed raw store for the `fsindex` provider.
//!
//! `open` runs the full DDL via [`dr::open`], `reset` truncates the
//! entity tables, and writes go through
//! [`datalib_etl::bulk::bulk_upsert_entity_in_tx`] — the
//! bookkeeping-free write path, since fsindex has no `_bookkeeping`
//! sidecars (see [`super::schema_raw::full_ddl`]).
//!
//! Branch handling: dolt is single-active-branch per connection, so
//! [`Self::checkout_branch`] switches the pool's one connection. It
//! tries `dolt_checkout(branch)` and falls back to
//! `dolt_checkout('-b', branch)` when that reports no such branch.
//!
//! Spelling matters here: doltlite exposes the dolt procedures as SQL
//! **functions**, so it is `SELECT dolt_checkout(?)`, never MySQL's
//! `CALL DOLT_CHECKOUT(?)` — the latter is a parse error
//! (`near "CALL": syntax error`) and, because both the primary and the
//! `-b` fallback used it, `--branch` failed outright rather than
//! degrading. `//datalib/backend/core/src/app_store.rs` documents the
//! same distinction for `dolt_commit`.
//!
//! The order is load-bearing in both directions: a plain checkout of a
//! branch that does not exist errors with `no such branch or table`,
//! and `-b` on one that does errors with `branch already exists`. So
//! the fallback has to be tried in that order, and neither call may be
//! treated as idempotent.
//!
//! Because a fresh connection starts on `main`, the checkout is only
//! true for as long as the pool keeps this connection. [`dr::open`]
//! pins `max_connections(1)` and disables connection recycling for
//! exactly that reason — see its "Connection pool size" docs.
//!
//! See [`super::schema_raw`] for the table shapes and
//! [`EXTRACT.md`](../../EXTRACT.md) §"Multi-root via doltlite branches"
//! for why the orchestrator may checkout a non-`main` branch before
//! the scan.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw as dr;

use super::schema_raw::{full_ddl, FileRow, ScanMetaRow, DATA_TABLES};

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

/// Per-`diff_type` row counts for the `files` table between a scan's
/// commit and its parent. `unchanged` rows are not counted here — they
/// fall out as `total_scanned - added - modified` at the call site.
#[derive(Debug, Default, Clone)]
pub struct DiffCounts {
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Truncate the entity tables so the next walk re-writes from
    /// scratch (the truncate-and-rebuild model). fsindex has no
    /// `_bookkeeping` sidecars (see [`super::schema_raw::full_ddl`]), so
    /// we can't use the shared [`dr::truncate_data_tables`] — which
    /// also deletes `<t>_bookkeeping` — and DELETE the entity tables
    /// directly. Whole-table bookkeeping (`sync_runs`) is left alone.
    pub async fn reset(&self) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin truncate tx")?;
        for table in DATA_TABLES {
            // Audited: `table` iterates a `&'static str` const array of our own
            // table names; no runtime data reaches the statement.
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("truncate {table}"))?;
        }
        tx.commit().await.context("commit truncate tx")?;
        Ok(())
    }

    /// Switch the open connection's active branch, creating it if it
    /// doesn't exist. See the module docs for the spelling and the
    /// ordering, both of which are load-bearing.
    ///
    /// Verifies the switch by reading `active_branch()` back. A
    /// checkout that returned `Ok` without moving would be the
    /// dangerous shape — the scan would go on to write every row to
    /// whatever branch it was already on and report success.
    pub async fn checkout_branch(&self, branch: &str) -> Result<()> {
        // `dolt_checkout(branch)` errors when the branch is absent, so
        // the error is the signal to create it rather than a failure.
        let existing = sqlx::query("SELECT dolt_checkout(?)")
            .bind(branch)
            .execute(&self.pool)
            .await;
        if let Err(no_such_branch) = existing {
            sqlx::query("SELECT dolt_checkout('-b', ?)")
                .bind(branch)
                .execute(&self.pool)
                .await
                .with_context(|| {
                    format!(
                        "dolt checkout -b {branch} (after checkout {branch}                          reported: {no_such_branch})"
                    )
                })?;
        }

        let active: String = sqlx::query("SELECT active_branch() AS b")
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("read back active branch after checkout {branch}"))?
            .try_get("b")
            .with_context(|| format!("decode active branch after checkout {branch}"))?;
        anyhow::ensure!(
            active == branch,
            "checkout of branch {branch:?} reported success but the connection              is on {active:?}"
        );
        Ok(())
    }

    /// Producer-consumer write path: one batch of content rows lands
    /// in a SINGLE sqlite transaction (no bookkeeping sidecars — see
    /// the module docs). The matching host observations go to the
    /// fingerprint cache, not here. These are sqlite-level `BEGIN…COMMIT`s
    /// that flush the working set; the single version-control
    /// `dolt_commit` happens once at end of scan (see [`Self::commit`]).
    /// Per-batch flushing keeps both our Rust memory and doltlite's
    /// in-transaction buffer bounded on a tens-of-millions-of-rows
    /// scan. Returns the wall time.
    pub async fn write_batch(&self, files: &[FileRow], _now: &str) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        if files.is_empty() {
            return Ok(started.elapsed());
        }
        let mut tx = self.pool.begin().await.context("begin batch tx")?;
        bulk_upsert_entity_in_tx(&mut tx, files).await?;
        tx.commit().await.context("commit batch tx")?;
        Ok(started.elapsed())
    }

    /// Compact the doltlite chunk store via `dolt_gc()`, reclaiming the
    /// immutable-chunk novelty accumulated across the scan's per-batch
    /// commits. Without this a large scan's on-disk size is dominated
    /// by write amplification (observed ~7 KB/row across hundreds of
    /// commits, vs ~1 KB/row of actual data). Returns the wall time.
    pub async fn gc(&self) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        sqlx::query("SELECT dolt_gc()")
            .execute(&self.pool)
            .await
            .context("dolt_gc")?;
        Ok(started.elapsed())
    }

    /// The one version-control commit per scan. Seals the whole
    /// truncate-and-rebuild working set into a single `dolt_log` entry,
    /// so `dolt diff HEAD^ HEAD` is exactly "what this scan changed,"
    /// and — crucially — the next [`RawDb::open`] sees a clean tree and
    /// skips the rescue commit. Returns the wall time.
    ///
    /// Cheap now that the bookkeeping schema no longer carries a
    /// `DEFAULT` clause (which made `dolt_commit` super-linear in
    /// doltlite v0.11.x — see `bookkeeping_ddl_for`): committing an
    /// unchanged rescan is a near-empty diff, and even a first scan of
    /// a million rows commits in a few seconds.
    pub async fn commit(&self, msg: &str) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(msg)
            .execute(&self.pool)
            .await
            .context("dolt_commit")?;
        Ok(started.elapsed())
    }

    /// Root-relative ids of every directory row, in id order. Used by
    /// the post-write stamping pass to decide which dirs need a UUID
    /// breadcrumb — bounded by the directory count, which is tiny next
    /// to the file count.
    pub async fn dir_ids(&self) -> Result<Vec<String>> {
        let ids =
            sqlx::query_scalar::<_, String>("SELECT id FROM files WHERE kind = 'dir' ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .context("select dir ids")?;
        Ok(ids)
    }

    /// Every entry id this scan wrote, root-relative.
    ///
    /// After the truncate-and-rebuild the `files` table is exactly what
    /// the walk saw, so this is the authoritative "still present" set —
    /// which is what the fingerprint cache is pruned against. Streamed
    /// rather than `fetch_all`ed so the row set is never materialised
    /// twice.
    pub async fn all_entry_ids(&self) -> Result<std::collections::BTreeSet<String>> {
        use futures::TryStreamExt;
        let mut out = std::collections::BTreeSet::new();
        let mut rows = sqlx::query("SELECT id FROM files").fetch(&self.pool);
        while let Some(r) = rows.try_next().await.context("stream file ids")? {
            out.insert(sqlx::Row::try_get::<String, _>(&r, 0).context("read id")?);
        }
        Ok(out)
    }

    /// Stamp one already-written directory row with its breadcrumb
    /// UUID. The row was written by the streaming pass; this is the
    /// explicit enrichment UPDATE the stamping pass issues after the
    /// breadcrumb file lands (see [`super`] stamping notes).
    pub async fn set_identity_uuid(&self, id: &str, uuid: &str) -> Result<()> {
        sqlx::query("UPDATE files SET identity_uuid = ? WHERE id = ?")
            .bind(uuid)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("update files.identity_uuid")?;
        Ok(())
    }

    /// Summarize what the most recent commit changed in `files`
    /// relative to its parent commit, read from doltlite's
    /// `dolt_diff_files` system table. Because the scan
    /// truncate-and-rebuilds, a row deleted and re-inserted identically
    /// hashes to the same prolly-tree entry and shows as `unchanged`
    /// (so it isn't counted) — only genuinely changed files surface as
    /// added/modified/removed.
    ///
    /// Returns `None` when there's no parent commit to diff against
    /// (the very first scan) or the diff can't otherwise be resolved.
    /// Best-effort — never fails the run.
    pub async fn diff_counts_since_parent(&self) -> Option<DiffCounts> {
        let rows = sqlx::query(
            "SELECT diff_type, COUNT(*) AS n FROM dolt_diff_files \
              WHERE from_ref = 'HEAD^' AND to_ref = 'HEAD' \
                AND diff_type != 'unchanged' GROUP BY diff_type",
        )
        .fetch_all(&self.pool)
        .await
        .ok()?;
        let mut c = DiffCounts::default();
        for r in rows {
            let diff_type: String = r.try_get("diff_type").ok()?;
            let n: i64 = r.try_get("n").unwrap_or(0);
            match diff_type.as_str() {
                "added" => c.added = n as u64,
                "modified" => c.modified = n as u64,
                "removed" => c.removed = n as u64,
                _ => {}
            }
        }
        Some(c)
    }

    /// Upsert the (single) `scan_meta` row for the source.
    pub async fn write_scan_meta(&self, row: &ScanMetaRow, _now: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin scan_meta tx")?;
        bulk_upsert_entity_in_tx(&mut tx, std::slice::from_ref(row)).await?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }
}
