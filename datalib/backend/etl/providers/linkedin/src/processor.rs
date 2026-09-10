//! Program-A `DataProcessor`s for the `linkedin` source.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_linkedin_config::LinkedinConfig;

use crate::ingest;

/// Ingest wave: always present — ingest the export CSVs (and
/// optionally photos).
pub fn plan_ingest(
    ctx: PlanContext,
    config: LinkedinConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let export = config
        .export
        .as_ref()
        .ok_or_else(|| anyhow!("linkedin source {name} missing `export.path`"))?;
    let input_path = export.path();
    let max_sequential_failures = config.common.download_params.max_sequential_failures();
    Ok(vec![Box::new(LinkedinIngest {
        id: format!("linkedin/{name}/download"),
        raw_path,
        input_path,
        fetch_photos: export.fetch_photos,
        // The shared give-up knob, baked in at plan time: stop the photo
        // sweep after this many consecutive failures.
        photo_max_consecutive_failures: max_sequential_failures,
    })])
}

/// LinkedIn's download processor. Owns its raw doltlite store end to end.
struct LinkedinIngest {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
    fetch_photos: bool,
    photo_max_consecutive_failures: u64,
}

#[async_trait]
impl DataProcessor for LinkedinIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = ingest::fetch(ingest::FetchOptions {
            db,
            input_path: self.input_path.clone(),
            fetch_photos: self.fetch_photos,
            // Piggyback the shared give-up knob: stop the photo sweep after
            // this many consecutive failures.
            photo_max_consecutive_failures: self.photo_max_consecutive_failures,
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "files={} rows={} parse_errors={}",
            s.files, s.rows, s.parse_errors,
        );
        Ok(session.finish(ctx, summary).await)
    }
}
