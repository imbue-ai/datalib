//! Program-A `DataProcessor`s for the chatgpt_api source (extract + translate).
//! The source owns its raw store; the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanCommon, RunCtx, SourcePlan};
use frankweiler_etl::raw_store::PoolCheckpoint;
use frankweiler_etl_chatgpt_config::{ChatgptApiSync, ChatgptConfig};

use crate::extract;

pub fn plan(common: PlanCommon, config: ChatgptConfig) -> Result<SourcePlan> {
    let PlanCommon { name, raw_path, .. } = common;
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(ChatgptRender {
        id: format!("chatgpt/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
    }));
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(ChatgptExtract {
            id: format!("chatgpt/{name}/extract"),
            raw_path,
            sync,
        }));
    }
    Ok(plan)
}

struct ChatgptExtract {
    id: String,
    raw_path: PathBuf,
    sync: ChatgptApiSync,
}

#[async_trait]
impl DataProcessor for ChatgptExtract {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = extract::RawDb::open(&extract::db_path_for(&self.raw_path)).await?;
        let pool = db.pool().clone();
        ctx.register_checkpoint(
            &self.id,
            PoolCheckpoint::new(
                pool.clone(),
                format!("extract {}: interrupted (Ctrl-C)", ctx.name),
            ),
        );
        let s = extract::fetch(extract::FetchOptions {
            db_path: self.raw_path.clone(),
            db: Some(db),
            max_pages: self.sync.max_pages.map(|v| v as usize),
            limit: self.sync.limit.map(|v| v as usize),
            sleep_between: Duration::ZERO,
            conv_uuids: self.sync.conv_uuids.clone(),
            fetched_at: Some(ctx.now.to_string()),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} errors={} listing={} requests={}",
            s.fetched, s.skipped, s.errors, s.listing, s.requests,
        );
        Ok(frankweiler_etl::raw_store::commit_and_close(pool, ctx.name, summary).await)
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
        use crate::render_and_index_md::{parse::parse, render::render_all};
        let cursor_path =
            frankweiler_etl::render_cursor::cursor_path(ctx.root, "chatgpt", &self.name);
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
