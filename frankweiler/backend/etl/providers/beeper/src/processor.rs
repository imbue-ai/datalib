//! Program-A `DataProcessor`s for the beeper source. Beeper contributes an
//! **extract** processor ([`BeeperExtract`] — reads Beeper Texts' on-disk
//! SQLite stores) when `sync:` is present, plus an always-present
//! **translate** processor ([`BeeperRender`]). [`plan`] builds the
//! [`SourcePlan`] the orchestrator drives.
//!
//! Storage ownership lives here, not in the orchestrator: [`BeeperExtract`]
//! opens its own raw doltlite store, registers an opaque [`PoolCheckpoint`]
//! for interrupt-safety, and issues its own post-extract `dolt_commit`. The
//! orchestrator never sees a pool or a commit.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::periodize::Period;
use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl_beeper_config::{BeeperConfig, BeeperSync};

use crate::extract;

/// Build beeper's [`SourcePlan`]: always a translate processor (which bakes in
/// the `period` parsed from config), plus an extract processor when `sync:` is
/// present (managed). The provider owns the period decision; the orchestrator
/// passes only the envelope-level [`PlanCommon`].
pub fn plan(common: PlanCommon, config: BeeperConfig) -> Result<SourcePlan> {
    let PlanCommon { name, raw_path, .. } = common;

    // The render period is parsed from config once, at plan time, and baked
    // into the translate processor. Defaults to month when absent.
    let period = Period::from_config(config.sync.as_ref().and_then(|s| s.period.as_deref()))
        .context("parse beeper period")?;

    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(BeeperRender {
        id: format!("beeper/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
        period,
    }));

    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(BeeperExtract {
            id: format!("beeper/{name}/extract"),
            raw_path,
            sync,
        }));
    }

    Ok(plan)
}

/// Beeper's extract processor. Owns its raw doltlite store end to end.
struct BeeperExtract {
    id: String,
    raw_path: PathBuf,
    sync: BeeperSync,
}

#[async_trait]
impl DataProcessor for BeeperExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = extract::db_path_for(&self.raw_path);
        let db = extract::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = extract::fetch(extract::FetchOptions {
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

/// Beeper's translate processor — reads the raw store and emits one rendered
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
        use crate::render_and_index_md::{parse::parse, render::render_all};
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
