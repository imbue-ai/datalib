//! Program-A `DataProcessor`s for the chatgpt_api source (download + render).
//! The source owns its raw store; the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx};
use frankweiler_etl_chatgpt_config::ChatgptRenderConfig;
use frankweiler_etl_chatgpt_config::{ChatgptApiSync, ChatgptConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(
    ctx: PlanContext,
    config: ChatgptConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(ChatgptDownload {
            id: format!("chatgpt/{name}/download"),
            raw_path,
            sync,
        }));
    }
    Ok(procs)
}

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ChatgptRenderConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    Ok(vec![Box::new(ChatgptRender {
        id: format!("chatgpt/{name}/render"),
        raw_path,
        name,
    })])
}

struct ChatgptDownload {
    id: String,
    raw_path: PathBuf,
    sync: ChatgptApiSync,
}

#[async_trait]
impl DataProcessor for ChatgptDownload {
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
            max_pages: self.sync.max_pages.map(|v| v as usize),
            limit: self.sync.limit.map(|v| v as usize),
            sleep_between: Duration::ZERO,
            since: self.sync.since.clone(),
            conv_uuids: self.sync.conv_uuids.clone(),
            fetched_at: Some(ctx.now.to_string()),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} out_of_scope={} errors={} listing={} requests={}",
            s.fetched, s.skipped, s.out_of_scope, s.errors, s.listing, s.requests,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct ChatgptRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for ChatgptRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read chatgpt render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("chatgpt parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("chatgpt render_all")?;
        Ok("rendered".into())
    }
}
