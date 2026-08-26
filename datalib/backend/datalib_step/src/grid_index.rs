//! The `grid_index` step type: Load, un-fused into a first-class
//! fan-in step — everything lands in the unified grid table.
//!
//! Rebuilds/refreshes `system/backend_index/db.doltlite_db` from
//! every stanza's `.grid_rows.json` sidecar tree via
//! [`datalib_etl::grid_index::build_grid_index`] — which already carries the
//! per-doc fingerprint skip, so an up-to-date index costs one scan.
//! Closes with a `dolt_commit`; the resulting commit hash is the
//! output's content version.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use datalib_etl::grid_index::{build_grid_index, init_schema};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::events::{Emitter, OutputClaim};

pub const OUT_REL: &str = "system/backend_index";

pub async fn run(
    data_root: &Path,
    now: Option<&str>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let db_path = datalib_core::layout::backend_index_db(data_root);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
        // The index is 100% rebuilt from the sidecar trees, so
        // cache-aware backups (`restic --exclude-caches` etc.) may
        // skip it.
        datalib_core::layout::mark_derived_cache(parent);
    }
    // Pool size 1: doltlite's HEAD pointer + working tree are
    // per-connection (see datalib_etl::doltlite_raw module docs).
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open doltlite at {}", db_path.display()))?;
    init_schema(&pool).await?;

    let progress = emitter.progress();
    let summary = build_grid_index(&pool, data_root, |m| progress.set_message(m), now)
        .await
        .context("build_grid_index from sidecar trees")?;
    tracing::info!(
        loaded = summary.markdowns_loaded,
        skipped = summary.markdowns_skipped,
        rows = summary.rows_inserted,
        "grid_index: build_grid_index done"
    );

    let msg = format!(
        "datalib-step grid_index: markdowns_loaded={} markdowns_skipped={} rows_inserted={}",
        summary.markdowns_loaded, summary.markdowns_skipped, summary.rows_inserted,
    );
    let commit = datalib_etl::doltlite_raw::commit_run(&pool, &msg)
        .await
        .context("grid_index commit")?;
    if let Some(h) = commit.as_deref() {
        tracing::info!(commit = h, "grid_index: committed");
    }
    pool.close().await;

    // The dolt commit hash is a faithful content version: HEAD only
    // advances when rows actually changed. Without doltlite
    // (stock-sqlite dev builds) there is no hash and we report nothing,
    // so the runner hashes the index instead.
    match commit {
        Some(version) => Ok(vec![OutputClaim {
            path: OUT_REL.to_string(),
            version,
        }]),
        None => Ok(vec![]),
    }
}
