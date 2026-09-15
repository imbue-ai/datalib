//! The `qmd_index` function: one group's collection in the shared qmd
//! store, brought in line with the group's `render_markdown` tree.
//!
//! Every source has one of these, so scoping a search to a source is a
//! collection qmd applies inside retrieval. The store itself is one
//! file for the whole root (`unified_index/qmd_index/qmd/index.sqlite`),
//! written by every group's `qmd_index` and `qmd_embed` step; the tree
//! this step's id names, `<group>/qmd_index/`, is empty.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// A root indexed before per-source collections carries qmd's one
/// `mirror` collection, which no group claims. Read from qmd's own
/// registry table; an unreadable or absent index yields `false`, and a
/// missed retirement costs a stale collection until the next run, not
/// a wrong index.
async fn legacy_collection_present(data_root: &Path) -> bool {
    let path = datalib_runtime::qmd::qmd_index_path(data_root);
    if !path.exists() {
        return false;
    }
    match read_collection_names(&path).await {
        Ok(names) => names
            .iter()
            .any(|n| n == datalib_qmd_indexer::LEGACY_COLLECTION_NAME),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "qmd: could not read collections");
            false
        }
    }
}

async fn read_collection_names(path: &Path) -> Result<Vec<String>> {
    // qmd's index is a plain SQLite database, unlike every `.doltlite_db`
    // in the tree. Read-only: the file belongs to the writers this step
    // is about to run.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(false)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    let rows = sqlx::query("SELECT name FROM store_collections")
        .fetch_all(&pool)
        .await;
    pool.close().await;
    Ok(rows
        .context("read store_collections")?
        .iter()
        .filter_map(|r| r.try_get::<String, _>("name").ok())
        .collect())
}

struct StepProgress(datalib_etl::progress::Progress);

impl datalib_qmd_indexer::IndexProgress for StepProgress {
    fn total(&self, files: u64) {
        self.0.metric("queued", &[], files as i64);
        self.0.metric("done", &[], 0);
    }
    fn done(&self, files: u64) {
        self.0.metric("done", &[], files as i64);
    }
}

pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("qmd index");
    let qmd_version = datalib_qmd_indexer::DEFAULT_QMD_VERSION;
    let models_dir = models_dir.unwrap_or_else(datalib_qmd_indexer::default_models_dir);
    datalib_qmd_indexer::prepare_store(data_root, &models_dir)?;

    let summary = datalib_qmd_indexer::index_one_group(
        data_root,
        &env.group,
        qmd_version,
        &StepProgress(progress.clone()),
    )
    .await
    .with_context(|| format!("index group {}", env.group))?;
    progress.metric("queued", &[], 0);
    for (name, n) in [
        ("indexed", summary.indexed),
        ("updated", summary.updated),
        ("unchanged", summary.unchanged),
        ("removed", summary.removed),
        ("documents", summary.documents),
    ] {
        progress.metric(name, &[], n as i64);
    }
    tracing::info!(
        group = %env.group,
        indexed = summary.indexed,
        updated = summary.updated,
        unchanged = summary.unchanged,
        removed = summary.removed,
        documents = summary.documents,
        "qmd_index: done"
    );

    // After the indexing pass, never before: retiring deletes content no
    // other collection names yet.
    if legacy_collection_present(data_root).await {
        datalib_qmd_indexer::retire_one_collection(
            data_root,
            qmd_version,
            datalib_qmd_indexer::LEGACY_COLLECTION_NAME,
        )
        .context("retire the legacy `mirror` collection")?;
    }

    // The store rebuilds from the render_markdown trees, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. Tag the
    // whole `unified_index/` tree for the same reason the grid step
    // does — one tag covers both indexes however they are ordered.
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));

    // The step's own tree holds nothing: what it wrote is a collection
    // in the shared store. The directory still exists, so the tree the
    // id names is there to be measured.
    std::fs::create_dir_all(data_root.join(&env.step))?;

    Ok(vec![OutputClaim {
        path: env.step.clone(),
        version: summary.version,
        rows: Some(summary.documents),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data root that has never synced has no index to read, and that
    /// is a normal state — nothing to retire, no error.
    #[tokio::test]
    async fn no_index_means_no_legacy_collection() {
        let td = tempfile::tempdir().unwrap();
        assert!(!legacy_collection_present(td.path()).await);
    }

    /// The migration this exists for: a root indexed before per-source
    /// collections carries `mirror`.
    #[tokio::test]
    async fn the_legacy_collection_is_noticed() {
        let td = tempfile::tempdir().unwrap();
        let path = datalib_runtime::qmd::qmd_index_path(td.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE store_collections (name TEXT PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        for name in ["slack_imbue", "deleted_source"] {
            sqlx::query("INSERT INTO store_collections (name) VALUES (?)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        assert!(!legacy_collection_present(td.path()).await);
        sqlx::query("INSERT INTO store_collections (name) VALUES ('mirror')")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert!(legacy_collection_present(td.path()).await);
    }
}
