//! Program-A `DataProcessor`s for the beeper source. Beeper contributes an
//! **download** processor ([`BeeperDownload`] — reads Beeper Texts' on-disk
//! SQLite stores) when `sync:` is present, plus an always-present
//! **render** processor ([`BeeperRender`]). [`plan_download`] /
//! [`plan_render`] build the per-wave processors the orchestrator drives.
//!
//! Storage ownership lives here, not in the orchestrator: [`BeeperDownload`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-download `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::periodize::Period;
use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_beeper_config::{BeeperConfig, BeeperSync};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: BeeperConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(BeeperDownload {
            id: format!("beeper/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave. NOTE: reads `sync.period` — the render period is parsed
/// from config once, at plan time, and baked into the render
/// processor (defaults to month when absent). `period` is a render
/// knob that historically lives in the `sync:` block; it moves out in
/// the config-format split.
pub fn plan_render(ctx: PlanContext, config: BeeperConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let period = Period::from_config(config.sync.as_ref().and_then(|s| s.period.as_deref()))
        .context("parse beeper period")?;
    Ok(vec![Box::new(BeeperRender {
        id: format!("beeper/{name}/render"),
        raw_path,
        name,
        period,
    })])
}

/// Beeper's download processor. Owns its raw doltlite store end to end.
struct BeeperDownload {
    id: String,
    raw_path: PathBuf,
    sync: BeeperSync,
}

#[async_trait]
impl DataProcessor for BeeperDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            sources: self.sync.sources.clone(),
            beeper_data_dir: self.sync.beeper_data_dir.clone(),
            media: self.sync.media,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "rooms={} users={} events={} blobs={} blob_errors={} enriched={} orphaned={}",
            s.rooms,
            s.users,
            s.events,
            s.blobs,
            s.blob_errors,
            s.events_enriched,
            s.events_orphaned,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

/// Beeper's render processor — reads the raw store and emits one rendered
/// markdown per `(room, period)` through the fused-Load callback.
struct BeeperRender {
    id: String,
    raw_path: PathBuf,
    name: String,
    period: Period,
}

#[async_trait]
impl DataProcessor for BeeperRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(&self.raw_path, self.period)
            .with_context(|| format!("beeper parse {}", self.raw_path.display()))?;
        let raw_db_path = frankweiler_etl::doltlite_raw::db_path_for(&self.raw_path);
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(
            &parsed,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &raw_db_path,
        )
        .context("beeper render_all")?;
        Ok("rendered".into())
    }
}
