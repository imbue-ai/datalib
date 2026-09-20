//! The ingest wave for the `facebook` source: its planner and the
//! `DataProcessor` it plans.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_facebook_config::FacebookConfig;

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: FacebookConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let export = config
        .export
        .as_ref()
        .ok_or_else(|| anyhow!("facebook source {name} missing `export.path`"))?;
    Ok(vec![Box::new(FacebookIngest {
        id: format!("facebook/{name}/download"),
        raw_path,
        input_path: export.path(),
    })])
}

/// Facebook's ingest processor. Owns its raw doltlite store end to end.
struct FacebookIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
}

#[async_trait]
impl DataProcessor for FacebookIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx
            .open_store_with_blobs(
                db.pool().clone(),
                db.cas().map(|cas| cas.pool().clone()),
                entity_db,
            )
            .await;
        let s = ingest::fetch(ingest::FetchOptions {
            db,
            input_path: self.input_path.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "files={} rows={} parse_errors={} media_stored={} media_known={} media_missing={}",
            s.files, s.rows, s.parse_errors, s.media_stored, s.media_known, s.media_missing,
        );
        session.finish(ctx, summary).await
    }
}
