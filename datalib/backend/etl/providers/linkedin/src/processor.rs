//! Program-A `DataProcessor`s for the `linkedin` source.

use datalib_etl::processor::RenderPass;
use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_linkedin_config::LinkedinConfig;
use datalib_etl_linkedin_config::LinkedinRenderConfig;

use crate::download;

/// Download wave: always present — ingest the export CSVs (and
/// optionally photos).
pub fn plan_download(
    ctx: PlanContext,
    config: LinkedinConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let input_path = config.common.input_or_raw_path().to_path_buf();
    let max_sequential_failures = config.common.download_params.max_sequential_failures();
    Ok(vec![Box::new(LinkedinDownload {
        id: format!("linkedin/{name}/download"),
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
    config: LinkedinRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(LinkedinRender {
        id: format!("linkedin/{name}/render"),
        raw_path,
        name,
    })])
}

/// LinkedIn's download processor. Owns its raw doltlite store end to end.
struct LinkedinDownload {
    id: String,
    raw_path: PathBuf,
    input_path: PathBuf,
    fetch_photos: bool,
    photo_max_consecutive_failures: u64,
}

#[async_trait]
impl DataProcessor for LinkedinDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let s = download::fetch(download::FetchOptions {
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

/// LinkedIn's render processor — renders the three feeds (messages,
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

    fn render_version(&self) -> Option<u32> {
        Some(crate::render::RENDER_VERSION)
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        // This renderer walks the whole raw store every run, so the set it
        // considered is the complete one: anything else the render store
        // holds is a document whose source is gone. The driver sweeps.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut on_doc = |md| ctx.emit_doc(md);

        // Every message-shaped feed (DMs + AI-coach transcripts) renders.
        let r_pass = crate::render::render(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin render")?;
        // Connections render as first-class contacts via the shared contact
        // renderer (sibling of the chat path above).
        let c_pass = crate::connections::render_connections(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin connections render")?;
        // Your own posts (Shares) and the comments you left, grouped one
        // chat-style thread per post, with linkouts back to linkedin.com.
        let p_pass = crate::posts::render_posts(
            &self.raw_path,
            ctx.root,
            &self.name,
            ctx.progress,
            ctx.prior_fingerprints,
            &mut on_doc,
            &mut seen,
        )
        .context("linkedin posts render")?;

        // One sweep over the union of all three feeds: each contributes a
        // slice of this source's documents, and sweeping per feed would
        // have each delete the other two's.
        // The sweep drops anything none of the three named, so it is only
        // safe when all three actually walked. One that bailed contributed
        // no uuids, and sweeping on that deletes what it would have named.
        let pass = if [c_pass, p_pass, r_pass]
            .iter()
            .all(|p| *p == RenderPass::Walked)
        {
            RenderPass::Walked
        } else {
            RenderPass::Skipped
        };
        ctx.retain_documents(pass, &seen);
        Ok("rendered".into())
    }
}
