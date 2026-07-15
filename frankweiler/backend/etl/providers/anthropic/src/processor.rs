//! Program-A `DataProcessor`s for the anthropic (claude_api / claude_export)
//! source. `claude_api` contributes extract + translate; `claude_export` is
//! translate-only (no `sync:`), sharing the same renderer. The source owns its
//! raw store (open/commit/checkpoint); the orchestrator only drives `run`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

use frankweiler_etl::processor::{DataProcessor, PlanContext, RunCtx, SourcePlan};
use frankweiler_etl_anthropic_config::{AnthropicConfig, ClaudeApiSync};

use crate::extract;

/// Build the SourcePlan: always a translate processor; an extract processor
/// when `sync:` is present (managed). `claude_export` has no `sync:`, so it
/// yields translate only.
pub fn plan(ctx: PlanContext, config: AnthropicConfig) -> Result<SourcePlan> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let mut plan = SourcePlan::new();
    plan.translate.push(Box::new(AnthropicRender {
        id: format!("anthropic/{name}/translate"),
        raw_path: raw_path.clone(),
        name: name.clone(),
    }));
    if let Some(sync) = config.sync {
        plan.extract.push(Box::new(AnthropicExtract {
            id: format!("anthropic/{name}/extract"),
            raw_path,
            sync,
        }));
    }
    Ok(plan)
}

struct AnthropicExtract {
    id: String,
    raw_path: PathBuf,
    sync: ClaudeApiSync,
}

#[async_trait]
impl DataProcessor for AnthropicExtract {
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
            // users.json is expected alongside the raw store (playback seeds it).
            export_dir: Some(self.raw_path.clone()),
            overlap: self
                .sync
                .refresh_most_recent_n_chat_count
                .map(|v| v as usize)
                .unwrap_or(0),
            sleep_between: Duration::ZERO,
            since: self.sync.since.clone(),
            conv_uuids: self.sync.conv_uuids.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let summary = format!(
            "fetched={} skipped={} out_of_scope={} errors={} forbidden_orgs={} total={} \
             requests={} forbidden_retry_attempts={} forbidden_retry_recoveries={}",
            s.fetched,
            s.skipped,
            s.out_of_scope,
            s.errors,
            s.forbidden_orgs,
            s.total,
            s.requests,
            s.forbidden_retry_attempts,
            s.forbidden_retry_recoveries,
        );
        Ok(session.finish(ctx, summary).await)
    }
}

struct AnthropicRender {
    id: String,
    raw_path: PathBuf,
    name: String,
}

#[async_trait]
impl DataProcessor for AnthropicRender {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        use crate::render_and_index_md::{parse::parse, render::render_all};
        let cursor_path = frankweiler_etl::render_cursor::cursor_path(ctx.root, &self.name);
        let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
            .with_context(|| format!("read anthropic render cursor {}", cursor_path.display()))?;
        let parsed = parse(
            &self.raw_path,
            cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
        )
        .with_context(|| format!("anthropic parse {}", self.raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        render_all(&parsed, ctx.root, &self.name, ctx.progress, &mut on_doc)
            .context("anthropic render_all")?;
        Ok("rendered".into())
    }
}
