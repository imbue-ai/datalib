//! Program-A `DataProcessor`s for the `linkedin` source.
//!
//! LinkedIn is a file-backed export ("takeout"): [`LinkedinExtract`] ingests
//! every CSV in the export into the raw store (optionally downloading
//! connection photos), and [`LinkedinRender`] renders the three translate
//! feeds (messages, connections, posts). The source owns its raw store
//! (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_linkedin_config::LinkedinConfig;

use crate::extract;

/// Download wave: always present — ingest the export CSVs (and
/// optionally photos).
pub fn plan_download(
    ctx: PlanContext,
    config: LinkedinConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let max_sequential_failures = config.common.extract_params.max_sequential_failures();
    Ok(vec![Box::new(LinkedinExtract {
        id: format!("linkedin/{name}/extract"),
        raw_path,
        input_path,
        fetch_photos: config.fetch_photos,
        // The shared give-up knob, baked in at plan time: stop the photo
        // sweep after this many consecutive failures.
        photo_max_consecutive_failures: max_sequential_failures,
    })])
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: LinkedinConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(LinkedinRender {
        id: format!("linkedin/{name}/translate"),
        raw_path,
        name,
    })])
}

/// LinkedIn's extract processor. Owns its raw doltlite store end to end.
struct LinkedinExtract {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
    fetch_photos: bool,
    photo_max_consecutive_failures: u64,
}

#[async_trait]
impl DataProcessor for LinkedinExtract {
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

/// LinkedIn's translate processor — renders the three feeds (messages,
/// connections, posts) and emits each rendered markdown through the
/// fused-Load callback.
struct LinkedinRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for LinkedinRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let mut on_doc = |md| ctx.emit_doc(md);

        // Every message-shaped feed (DMs + AI-coach transcripts) renders.
        crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("linkedin render")?;
        // Connections render as first-class contacts via the shared contact
        // renderer (sibling of the chat path above).
        crate::connections::render_connections(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("linkedin connections render")?;
        // Your own posts (Shares) and the comments you left, grouped one
        // chat-style thread per post, with linkouts back to linkedin.com.
        crate::posts::render_posts(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
        )
        .context("linkedin posts render")?;

        Ok("rendered".into())
    }
}
