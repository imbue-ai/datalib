//! The render wave for the claude source: its planner and the
//! [`RenderProcessor`] it plans.

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::processor::PlanContext;
use datalib_etl_claude_config::ClaudeRenderConfig;
use datalib_etl_render::processor::{plan_source_render, RenderCtx, RenderProcessor, SourceRender};
use std::path::Path;

/// Render wave: always present (renders whatever is in the raw store).
pub fn plan_render(
    ctx: PlanContext,
    config: ClaudeRenderConfig,
) -> Result<Vec<Box<dyn RenderProcessor>>> {
    Ok(plan_source_render(
        ctx,
        config.common.raw_path(),
        ClaudeRender {
            max_project_doc_bytes: config.max_project_doc_bytes,
        },
    ))
}

struct ClaudeRender {
    /// See [`ClaudeRenderConfig::max_project_doc_bytes`].
    max_project_doc_bytes: Option<usize>,
}

#[async_trait]
impl SourceRender for ClaudeRender {
    const PROVIDER: &'static str = "claude";

    fn render_version(&self) -> u32 {
        crate::render::render::RENDER_VERSION
    }

    fn render_params(&self) -> serde_json::Value {
        datalib_etl_chat_common::render::layout_params()
    }

    async fn run(&self, raw_path: &Path, ctx: &RenderCtx<'_>) -> Result<String> {
        use crate::render::{parse::parse, render::render_all};
        let parsed = parse(raw_path, ctx.name, ctx.raw_range())
            .with_context(|| format!("claude parse {}", raw_path.display()))?;
        let mut on_doc = |md| ctx.emit_doc(md);
        let buckets = render_all(
            &parsed,
            ctx.root,
            ctx.name,
            crate::render::render::RenderOptions {
                max_project_doc_bytes: self.max_project_doc_bytes,
            },
            ctx.progress,
            &mut on_doc,
        )
        .context("claude render_all")?;
        // A bucket this run looked at is a conversation or a project;
        // both uuids are declared with nothing, so whichever page it had
        // that this run did not produce goes. The rendered ones follow
        // and replace that.
        for bucket in parsed.scan.render.iter().flatten() {
            ctx.declare_bucket(
                &crate::render::ids::conversation(ctx.name, bucket).uuid,
                &[],
            )?;
            ctx.declare_bucket(&crate::render::ids::project(ctx.name, bucket).uuid, &[])?;
        }
        ctx.declare_empty(parsed.scan.gone.iter().map(String::as_str))?;
        ctx.finish(&buckets, parsed.scan.new_head.as_deref())?;
        Ok("rendered".into())
    }
}
