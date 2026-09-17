//! The `grid_index` function: every source's render store, stacked into
//! the unified grid table at `unified_index/grid_index`.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::grid_index::{build_grid_index_for, open_index};

use crate::events::{Emitter, OutputClaim};
use crate::source::StepEnv;

/// The one tree this step writes, `unified_index/grid_index`, as the
/// applet that reads it resolves it from the data root.
pub fn out_rel() -> String {
    format!(
        "{}/{}",
        datalib_core::layout::UNIFIED_INDEX_DIR,
        datalib_core::layout::GRID_DIR
    )
}

pub async fn run(
    data_root: &Path,
    env: &StepEnv,
    now: Option<&str>,
    emitter: &Emitter,
) -> Result<Vec<OutputClaim>> {
    let db_path = datalib_core::layout::grid_index_db(data_root);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
        // Tag the whole `unified_index/` tree, not just this step's own
        // directory: every index under it is rebuilt from the sidecar
        // trees, so cache-aware backups (`restic --exclude-caches` etc.)
        // may skip all of it. Tagging the parent also means the tag is
        // right before the qmd step has ever run. Nothing precious lives
        // here — feedback and the job queue are under `system/`, which is
        // never tagged.
        datalib_core::layout::mark_derived_cache(&datalib_core::layout::unified_index_dir(
            data_root,
        ));
    }
    let pool = open_index(&db_path).await?;

    // The stores to read come from the graph, not from a directory scan:
    // the same rule the qmd step follows, so the two indexes agree on
    // what a source is.
    let sources = crate::qmd_index::groups_from_inputs(&env.inputs);
    tracing::info!(sources = ?sources, "grid_index: the render stores the graph names");
    let progress = emitter.progress();
    let summary =
        build_grid_index_for(&pool, data_root, &sources, |m| progress.set_message(m), now)
            .await
            .context("stack the per-source render stores")?;
    tracing::info!(
        // `read` is the one that says whether the cursors are working:
        // it is how many documents were pulled out of the per-source
        // stores at all, and on a steady-state run it should be 0.
        read = summary.markdowns_total,
        loaded = summary.markdowns_loaded,
        removed = summary.markdowns_removed,
        rows = summary.rows_inserted,
        "grid_index: build_grid_index done"
    );

    for (name, n) in [
        ("markdowns_read", summary.markdowns_total),
        ("markdowns_loaded", summary.markdowns_loaded),
        ("markdowns_removed", summary.markdowns_removed),
        ("rows_inserted", summary.rows_inserted),
    ] {
        progress.metric(name, &[], n as i64);
    }

    let msg = format!(
        "datalib-step grid_index: markdowns_read={} markdowns_loaded={} \
         markdowns_removed={} rows_inserted={}",
        summary.markdowns_total,
        summary.markdowns_loaded,
        summary.markdowns_removed,
        summary.rows_inserted,
    );
    let commit = datalib_etl::doltlite_raw::commit_run(&pool, &msg)
        .await
        .context("grid_index commit")?;
    if let Some(h) = commit.as_deref() {
        tracing::info!(commit = h, "grid_index: committed");
    }
    // HEAD, not the commit this run happened to make: `commit_run`
    // returns `None` both without doltlite *and* when the working tree
    // was already clean. Reporting no version in the clean case would
    // drop us to the tree hash — a digest from a different hash space
    // than the dolt hash reported last time — so every no-op run after
    // a real change would read as changed.
    let version = datalib_etl::doltlite_raw::head_commit(&pool)
        .await
        .context("grid_index head")?;
    pool.close().await;

    // The dolt commit hash is a faithful content version: HEAD only
    // advances when rows actually changed. Without doltlite
    // (stock-sqlite dev builds) there is no hash and we report nothing,
    // so the runner hashes the index instead.
    match version {
        Some(version) => Ok(vec![OutputClaim {
            path: out_rel(),
            version,
            rows: None,
        }]),
        None => Ok(vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The step marks the whole `unified_index/` tree as derived cache, and
    /// leaves `system/` alone.
    #[tokio::test]
    async fn grid_index_marks_the_index_tree_as_derived_cache() {
        let td = tempfile::tempdir().unwrap();
        let data_root = td.path();

        let env = StepEnv {
            step: "unified_index/grid_index".into(),
            group: "unified_index".into(),
            group_type: None,
            function: crate::function::Function::GridIndex,
            inputs: Vec::new(),
        };
        run(
            data_root,
            &env,
            Some("2026-01-01T00:00:00+00:00"),
            &Emitter::new("test".into()),
        )
        .await
        .expect("grid_index over an empty data root should succeed");

        assert!(
            data_root.join("unified_index/CACHEDIR.TAG").is_file(),
            "unified_index/ must be tagged, so one tag covers both index trees"
        );
        assert!(
            !data_root
                .join("unified_index/grid_index/CACHEDIR.TAG")
                .exists(),
            "the per-index tag was replaced by the one on the parent"
        );
        // `system/` is operational history — feedback, the job queue — and is
        // not rebuildable from raw, so it must never be swept up by
        // `--exclude-caches`.
        assert!(
            !data_root.join("system/CACHEDIR.TAG").exists(),
            "system/ must never be tagged as cache"
        );
    }
}
