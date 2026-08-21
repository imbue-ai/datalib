//! Program-A `DataProcessor` for the `lightroom` source.
//!
//! `lightroom` is **download-only** for now — it mirrors a catalog into
//! a doltlite store and renders nothing. So [`plan_download`]
//! contributes a single download processor and [`plan_render`] returns
//! an empty vec; "download-only" is structural (a missing processor),
//! not a flag. Same shape as `fsindex`.
//!
//! The render side is deliberately deferred rather than stubbed: a photo
//! is not chat-shaped, and the useful projection — one `grid_rows` row
//! per image, joining `Adobe_images` / `AgLibraryFile` / `AgLibraryFolder`
//! for the path, `AgHarvestedExifMetadata` for capture time and camera,
//! `AgLibraryKeywordImage` for keywords — plus some way to surface the
//! actual pictures, is its own design question. See `INGEST.md`
//! §"What render will need".
//!
//! The source owns its raw store end to end (open, DDL, write, commit,
//! interrupt `Checkpoint`) through the standard
//! [`RawStoreSession`](datalib_etl::raw_store::RawStoreSession); the
//! orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl::raw_layout;
use datalib_etl_lightroom_config::{LightroomConfig, LightroomRenderConfig};

use crate::download::{self, MirrorOptions};

/// Build the engine options from the source's config.
pub fn mirror_options(config: &LightroomConfig) -> MirrorOptions {
    MirrorOptions {
        source_path: config.common.input_or_raw_path().to_path_buf(),
        snapshot: config.snapshot,
        include_tables: config.include_tables.clone(),
        exclude_tables: config.exclude_tables.clone(),
        exclude_columns: config.effective_excluded_columns(),
        stable_key_columns: config.stable_key_columns.clone(),
        primary_keys: config.primary_keys.clone(),
        gc: config.gc,
    }
}

/// Download wave: the catalog mirror into the raw store.
pub fn plan_download(
    ctx: PlanContext,
    config: LightroomConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    Ok(vec![Box::new(LightroomDownload {
        id: format!("lightroom/{name}/download"),
        raw_path: config.common.raw_path().to_path_buf(),
        options: mirror_options(&config),
    })])
}

/// Render wave: `lightroom` is download-only, so this is always empty.
pub fn plan_render(
    ctx: PlanContext,
    config: LightroomRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let _ = (ctx, config);
    Ok(Vec::new())
}

/// The mirror processor. Owns its doltlite store end to end (open,
/// register the interrupt hook, mirror, commit + close via
/// `session.finish`).
struct LightroomDownload {
    id: String,
    raw_path: PathBuf,
    options: MirrorOptions,
}

#[async_trait]
impl DataProcessor for LightroomDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = raw_layout::entities_db(&self.raw_path);
        let pool = download::mirror::open_mirror(&entity_db).await?;
        let session = ctx.open_store(pool.clone(), entity_db).await;
        let stats = download::fetch(download::FetchOptions {
            mirror_path: self.raw_path.clone(),
            pool: Some(pool),
            options: self.options.clone(),
            progress: ctx.progress.clone(),
        })
        .await?;
        Ok(session.finish(ctx, stats.summary()).await)
    }
}
