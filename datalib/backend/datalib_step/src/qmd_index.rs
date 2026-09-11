//! The `qmd_index` function: the qmd search index over every
//! `render_markdown` tree, written to `unified_index/qmd_index`.
//!
//! One qmd collection per group, so a search scoped to one source is a
//! filter qmd applies inside retrieval rather than one the applet applies
//! to a global top-N.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The one tree this step writes, as the applet that reads it resolves
/// it from the data root.
pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
        datalib_core::layout::QMD_DIR
    )
}

/// The groups this step indexes, read off its declared inputs.
///
/// An input is a step id, and a step id is the tree it writes, so each
/// one reads `<group>/render_markdown` — the group is its first segment.
/// Taking the list from the graph rather than from a directory scan means
/// a source dropped from the config stops being indexed on the next run,
/// even while its rendered tree is still on disk.
fn groups_from_inputs(inputs: &[String]) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for input in inputs {
        if let Some(group) = input.split('/').next() {
            if !group.is_empty() {
                out.insert(group.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Collections the index still holds that no group claims any more:
/// the pre-per-source `mirror`, and any source since removed from the
/// config. Read from qmd's own registry table.
///
/// An unreadable or absent index yields none. This runs before qmd does,
/// so the answer is only ever used to *retire* a collection, and a
/// missed one costs a stale collection until the next run — not a wrong
/// index. Failing the step over it would be worse.
async fn collections_to_retire(data_root: &Path, keep: &[String]) -> Vec<String> {
    let path = datalib_runtime::qmd::qmd_index_path(data_root);
    if !path.exists() {
        return Vec::new();
    }
    let found = match read_collection_names(&path).await {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "qmd: could not read collections");
            return Vec::new();
        }
    };
    let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
    found
        .into_iter()
        .filter(|name| !keep.contains(name.as_str()))
        .collect()
}

async fn read_collection_names(path: &Path) -> Result<Vec<String>> {
    // qmd's index is a plain SQLite database, unlike every `.doltlite_db`
    // in the tree. Read-only: the file belongs to the qmd subprocess this
    // step is about to run.
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

pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    models_dir: Option<PathBuf>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let progress = emitter.progress();
    progress.set_message("qmd index");
    let groups = groups_from_inputs(&env.inputs);
    let retire = collections_to_retire(data_root, &groups).await;
    if !retire.is_empty() {
        tracing::info!(collections = ?retire, "qmd: retiring collections no group claims");
    }
    let mut opts = datalib_qmd_indexer::IndexOptions::new(data_root);
    opts.groups = groups;
    opts.descriptions = env.group_descriptions.clone();
    opts.retire_collections = retire;
    if let Some(d) = models_dir {
        opts.models_dir = d;
    }
    // run_index shells out to qmd; blocking work.
    let outcome = tokio::task::spawn_blocking(move || datalib_qmd_indexer::run_index(&opts))
        .await
        .context("qmd task panicked")??;
    tracing::info!(index = %outcome.index_path.display(), "qmd: done");
    // The index rebuilds from the render_markdown trees, so cache-aware
    // backups (`restic --exclude-caches` etc.) may skip it. Tag the
    // whole `unified_index/` tree for the same reason the grid step
    // does — one tag covers both indexes however they are ordered.
    datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(data_root));

    // qmd's sqlite gets touched on every pass, so any version we could
    // derive would move even when nothing was indexed. Report nothing:
    // the step is a leaf (nothing consumes unified_index/qmd_index
    // downstream), so the runner's fallback hash is never read by anyone
    // and the imprecision costs nothing.
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An input is a step id, which is also the tree it writes. The
    /// group is its first segment — not the whole string, and not a
    /// directory listing.
    #[test]
    fn groups_come_from_the_first_segment_of_each_input() {
        let inputs = vec![
            "slack_imbue/render_markdown".to_string(),
            "claude_personal/render_markdown".to_string(),
        ];
        assert_eq!(
            groups_from_inputs(&inputs),
            vec!["claude_personal".to_string(), "slack_imbue".to_string()]
        );
    }

    /// Two steps under one group collapse to one collection, and the
    /// list is deduped and ordered so a config reshuffle doesn't churn
    /// the collection set.
    #[test]
    fn groups_are_deduped_and_sorted() {
        let inputs = vec![
            "b/render_markdown".to_string(),
            "a/render_markdown".to_string(),
            "a/ingest".to_string(),
            String::new(),
        ];
        assert_eq!(
            groups_from_inputs(&inputs),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    /// A data root that has never synced has no index to read, and that
    /// is a normal state — nothing to retire, no error.
    #[tokio::test]
    async fn no_index_means_nothing_to_retire() {
        let td = tempfile::tempdir().unwrap();
        assert!(collections_to_retire(td.path(), &["a".to_string()])
            .await
            .is_empty());
    }

    /// The migration this exists for: a root indexed before per-source
    /// collections carries `mirror`, which no group claims.
    #[tokio::test]
    async fn legacy_and_orphaned_collections_are_retired() {
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
        for name in ["mirror", "slack_imbue", "deleted_source"] {
            sqlx::query("INSERT INTO store_collections (name) VALUES (?)")
                .bind(name)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;

        let mut retire = collections_to_retire(td.path(), &["slack_imbue".to_string()]).await;
        retire.sort();
        assert_eq!(
            retire,
            vec!["deleted_source".to_string(), "mirror".to_string()]
        );
    }
}
