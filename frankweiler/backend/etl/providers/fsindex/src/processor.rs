//! Program-A `DataProcessor` for the `fsindex` source.
//!
//! fsindex is **extract-only** — it indexes a directory tree into a doltlite
//! raw store and has no translate/render side (filesystem entries aren't
//! chat-shaped; see the crate-level docs). So [`plan`] contributes a single
//! extract processor and leaves `translate` empty — "extract-only" is
//! structural (a missing processor), not a flag.
//!
//! The source owns its raw store end to end (open, DDL, write, commit,
//! interrupt `Checkpoint`) through the standard
//! [`RawStoreSession`](frankweiler_etl::raw_store::RawStoreSession); the
//! orchestrator only drives `run`. The bespoke `dolt_gc` + provenance commit
//! message that the standalone `fsindex` CLI emits stay exclusive to that
//! binary — under the orchestrator the uniform per-source `dolt_commit` from
//! `session.finish` is the durable result (gc is best-effort, and the
//! config-driven scans are small).

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx, SourcePlan};
use frankweiler_etl::raw_layout;
use frankweiler_etl_fsindex_config::FsindexConfig;

use crate::extract;

/// Build the SourcePlan: a single extract processor (file-backed scan of the
/// tree at `input_path`). No translate — fsindex renders nothing.
pub fn plan(ctx: PlanContext, config: FsindexConfig) -> Result<SourcePlan> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let root = config.common.input_or_raw_path().to_path_buf();

    let mut plan = SourcePlan::new();
    plan.extract.push(Box::new(FsindexExtract {
        id: format!("fsindex/{name}/extract"),
        raw_path,
        root,
        source_name: name,
        stamp: config.stamp,
    }));
    Ok(plan)
}

/// fsindex's extract processor. Owns its raw doltlite store end to end (open,
/// register interrupt hook, scan the tree, commit+close).
struct FsindexExtract {
    id: String,
    raw_path: PathBuf,
    root: PathBuf,
    source_name: String,
    stamp: bool,
}

#[async_trait]
impl DataProcessor for FsindexExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = extract::fetch(extract::FetchOptions {
            // Unused when `db` is Some (fetch reuses the open handle); kept for
            // the standalone-open path's signature.
            db_path: self.raw_path.clone(),
            db: Some(db),
            source_name: self.source_name.clone(),
            root: self.root.clone(),
            // Branch selection is the standalone CLI's concern; the
            // orchestrator scans the source's default branch.
            target_doltlite_branch: None,
            no_stamp: !self.stamp,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "entries={} files_hashed={} files_reused={} dirs={} symlinks={} stamped={} errors={}",
            s.entries_scanned,
            s.files_hashed,
            s.files_reused,
            s.dirs,
            s.symlinks,
            s.stamped_directories,
            s.errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
