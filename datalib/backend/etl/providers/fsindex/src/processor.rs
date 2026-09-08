//! Program-A `DataProcessor` for the `fsindex` source.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_fsindex_config::FsindexConfig;
use datalib_etl_fsindex_config::FsindexRenderConfig;

use crate::download;

pub fn plan_download(
    ctx: PlanContext,
    config: FsindexConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let root = config.common.input_or_raw_path().to_path_buf();
    Ok(vec![Box::new(FsindexDownload {
        id: format!("fsindex/{name}/download"),
        raw_path,
        root,
        source_id: name,
        stamp: config.stamp,
    })])
}

/// Render wave: fsindex is download-only (it indexes the tree, renders
/// nothing), so this is always empty.
pub fn plan_render(
    ctx: PlanContext,
    config: FsindexRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let _ = (ctx, config);
    Ok(Vec::new())
}

/// fsindex's download processor. Owns its raw doltlite store end to end (open,
/// register interrupt hook, scan the tree, commit+close).
struct FsindexDownload {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    source_id: String,
    stamp: bool,
}

#[async_trait]
impl DataProcessor for FsindexDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        // The fingerprint cache is host state, so it lives in this
        // machine's cache directory — never in the data root, which may
        // be synced or copied between machines.
        let cache = FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
        tracing::info!(
            event = "fsindex_cache_open",
            path = %cache.path().display(),
            "reading this host's fingerprint cache from {}",
            cache.path().display(),
        );
        let s = download::fetch(download::FetchOptions {
            // Unused when `db` is Some (fetch reuses the open handle); kept for
            // the standalone-open path's signature.
            db_path: self.raw_path.clone(),
            db,
            source_id: self.source_id.clone(),
            root: self.root.clone(),
            // Branch selection is the standalone CLI's concern; the
            // orchestrator scans the source's default branch.
            target_doltlite_branch: None,
            cache,
            no_stamp: !self.stamp,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "entries={} files_hashed={} files_reused={} dirs={} symlinks={} stamped={} \
             errors={} cache_read={} cache_wrote={} cache_forgot={} cache_bytes={}",
            s.entries_scanned,
            s.files_hashed,
            s.files_reused,
            s.dirs,
            s.symlinks,
            s.stamped_directories,
            s.errors,
            s.cache_entries_loaded,
            s.cache_entries_written,
            s.cache_entries_forgotten,
            download::human_growth(s.cache_bytes_before, s.cache_bytes_after),
        );
        Ok(session.finish(ctx, summary).await)
    }
}
