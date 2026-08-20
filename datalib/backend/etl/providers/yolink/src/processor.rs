//! Program-A `DataProcessor`s for the yolink source (download + render).
//! The source owns its raw store (open/commit/checkpoint); the orchestrator
//! only drives `run`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_yolink_config::YolinkRenderConfig;
use datalib_etl_yolink_config::{YolinkConfig, YolinkSync};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: YolinkConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(YolinkDownload {
            id: format!("yolink/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave: always present. Renders whatever is in the raw store
/// into the single timeseries page (see [`crate::render`]); the page's
/// own HEAD-vs-cursor check decides whether there is work to do, so
/// planning it unconditionally costs one `dolt_log()` query on a
/// no-op run.
pub fn plan_render(
    ctx: PlanContext,
    config: YolinkRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(YolinkRender {
        id: format!("yolink/{name}/render"),
        raw_path,
        name,
    })])
}

struct YolinkDownload {
    id: String,
    raw_path: PathBuf,
    sync: YolinkSync,
}

#[async_trait]
impl DataProcessor for YolinkDownload {
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
            sync: self.sync.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "devices={} windows={} readings={} errors={} requests={}",
            s.devices, s.windows, s.readings, s.errors, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct YolinkRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for YolinkRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::parse::{parse, Parsed};
        use crate::render::render::{cursor_params, render_all};

        let cursor_path = datalib_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = datalib_etl::render_cursor::read_for_params(&cursor_path, &cursor_params())
            .with_context(|| format!("read yolink render cursor {}", cursor_path.display()))?;

        match parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("yolink parse {}", self.raw_path.display()))?
        {
            // The whole store is one document, so an unchanged HEAD means
            // an unchanged page — nothing was appended, nothing to draw.
            Parsed::UpToDate { head } => {
                tracing::info!(
                    event = "yolink_render_skipped",
                    source = %self.name,
                    head = %head,
                    "raw store HEAD unchanged since last render",
                );
                Ok(format!("up to date at {head}"))
            }
            Parsed::Fresh(parsed) => {
                let mut on_doc = |md| ctx.emit_doc(md);
                let s = render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
                    .context("yolink render_all")?;
                Ok(format!(
                    "devices={} series={} points={} plots={}",
                    s.devices, s.series, s.points, s.plots,
                ))
            }
        }
    }
}
